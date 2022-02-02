use std::{
    borrow::Borrow,
    hash::{BuildHasher, Hash, Hasher},
    mem::{self, MaybeUninit},
    ptr,
    sync::atomic::{self, AtomicUsize, Ordering},
};

use crossbeam_epoch::{Atomic, CompareAndSetError, Guard, Owned, Shared};

pub(crate) const BUCKET_ARRAY_DEFAULT_LENGTH: usize = 128;

pub(crate) struct BucketArray<K, V> {
    pub(crate) buckets: Box<[Atomic<Bucket<K, V>>]>,
    pub(crate) next: Atomic<BucketArray<K, V>>,
    pub(crate) epoch: usize,
    pub(crate) tombstone_count: AtomicUsize,
}

impl<K, V> Default for BucketArray<K, V> {
    fn default() -> Self {
        Self::with_length(0, BUCKET_ARRAY_DEFAULT_LENGTH)
    }
}

impl<K, V> BucketArray<K, V> {
    pub(crate) fn with_length(epoch: usize, length: usize) -> Self {
        assert!(length.is_power_of_two());
        let mut buckets = Vec::with_capacity(length);

        unsafe {
            ptr::write_bytes(buckets.as_mut_ptr(), 0, length);
            buckets.set_len(length);
        }

        let buckets = buckets.into_boxed_slice();

        #[cfg(feature = "unstable-debug-counters")]
        {
            use crate::sync::debug_counters::InternalGlobalDebugCounters as Counters;

            let size = (buckets.len() * std::mem::size_of::<Atomic<Bucket<K, V>>>()) as u64;
            Counters::bucket_array_created(size);
        }

        Self {
            buckets,
            next: Atomic::null(),
            epoch,
            tombstone_count: Default::default(),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        assert!(self.buckets.len().is_power_of_two());

        self.buckets.len() / 2
    }
}

#[cfg(feature = "unstable-debug-counters")]
impl<K, V> Drop for BucketArray<K, V> {
    fn drop(&mut self) {
        use crate::sync::debug_counters::InternalGlobalDebugCounters as Counters;

        let size = (self.buckets.len() * std::mem::size_of::<Atomic<Bucket<K, V>>>()) as u64;
        Counters::bucket_array_dropped(size);
    }
}

impl<'g, K: 'g + Eq, V: 'g> BucketArray<K, V> {
    pub(crate) fn get<Q: ?Sized + Eq>(
        &self,
        guard: &'g Guard,
        hash: u64,
        key: &Q,
    ) -> Result<Shared<'g, Bucket<K, V>>, RelocatedError>
    where
        K: Borrow<Q>,
    {
        let loop_result = self.probe_loop(guard, hash, |_, _, this_bucket_ptr| {
            let this_bucket_ref = if let Some(this_bucket_ref) = unsafe { this_bucket_ptr.as_ref() }
            {
                this_bucket_ref
            } else {
                return ProbeLoopAction::Return(Shared::null());
            };

            let this_key = &this_bucket_ref.key;

            if this_key.borrow() != key {
                return ProbeLoopAction::Continue;
            }

            let result_ptr = if this_bucket_ptr.tag() & TOMBSTONE_TAG == 0 {
                this_bucket_ptr
            } else {
                Shared::null()
            };

            ProbeLoopAction::Return(result_ptr)
        });

        match loop_result {
            ProbeLoopResult::Returned(t) => Ok(t),
            ProbeLoopResult::LoopEnded => Ok(Shared::null()),
            ProbeLoopResult::FoundSentinelTag => Err(RelocatedError),
        }
    }

    pub(crate) fn remove_if<Q: ?Sized + Eq, F: FnMut(&K, &V) -> bool>(
        &self,
        guard: &'g Guard,
        hash: u64,
        key: &Q,
        mut condition: F,
    ) -> Result<Shared<'g, Bucket<K, V>>, F>
    where
        K: Borrow<Q>,
    {
        let loop_result = self.probe_loop(guard, hash, |_, this_bucket, this_bucket_ptr| {
            let this_bucket_ref = if let Some(this_bucket_ref) = unsafe { this_bucket_ptr.as_ref() }
            {
                this_bucket_ref
            } else {
                return ProbeLoopAction::Return(Shared::null());
            };

            let this_key = &this_bucket_ref.key;

            if this_key.borrow() != key {
                return ProbeLoopAction::Continue;
            } else if this_bucket_ptr.tag() & TOMBSTONE_TAG != 0 {
                return ProbeLoopAction::Return(Shared::null());
            }

            let this_value = unsafe { &*this_bucket_ref.maybe_value.as_ptr() };

            if !condition(this_key, this_value) {
                return ProbeLoopAction::Return(Shared::null());
            }

            let new_bucket_ptr = this_bucket_ptr.with_tag(TOMBSTONE_TAG);

            match this_bucket.compare_and_set_weak(
                this_bucket_ptr,
                new_bucket_ptr,
                (Ordering::Release, Ordering::Relaxed),
                guard,
            ) {
                Ok(_) => ProbeLoopAction::Return(new_bucket_ptr),
                Err(_) => ProbeLoopAction::Reload,
            }
        });

        match loop_result {
            ProbeLoopResult::Returned(t) => Ok(t),
            ProbeLoopResult::LoopEnded => Ok(Shared::null()),
            ProbeLoopResult::FoundSentinelTag => Err(condition),
        }
    }

    // https://rust-lang.github.io/rust-clippy/master/index.html#type_complexity
    #[allow(clippy::type_complexity)]
    pub(crate) fn insert_or_modify<F: FnOnce() -> V, G: FnMut(&K, &V) -> V>(
        &self,
        guard: &'g Guard,
        hash: u64,
        state: InsertOrModifyState<K, V, F>,
        mut modifier: G,
    ) -> Result<Shared<'g, Bucket<K, V>>, (InsertOrModifyState<K, V, F>, G)> {
        let mut maybe_state = Some(state);

        let loop_result = self.probe_loop(guard, hash, |_, this_bucket, this_bucket_ptr| {
            let state = maybe_state.take().unwrap();

            let (new_bucket, maybe_insert_value) =
                if let Some(this_bucket_ref) = unsafe { this_bucket_ptr.as_ref() } {
                    let this_key = &this_bucket_ref.key;

                    if this_key != state.key() {
                        maybe_state = Some(state);

                        return ProbeLoopAction::Continue;
                    }

                    if this_bucket_ptr.tag() & TOMBSTONE_TAG == 0 {
                        let this_value = unsafe { &*this_bucket_ref.maybe_value.as_ptr() };
                        let new_value = modifier(this_key, this_value);

                        let (new_bucket, insert_value) = state.into_modify_bucket(new_value);

                        (new_bucket, Some(insert_value))
                    } else {
                        (state.into_insert_bucket(), None)
                    }
                } else {
                    (state.into_insert_bucket(), None)
                };

            if let Err(CompareAndSetError { new, .. }) = this_bucket.compare_and_set_weak(
                this_bucket_ptr,
                new_bucket,
                (Ordering::Release, Ordering::Relaxed),
                guard,
            ) {
                maybe_state = Some(InsertOrModifyState::from_bucket_value(
                    new,
                    maybe_insert_value,
                ));

                ProbeLoopAction::Reload
            } else {
                ProbeLoopAction::Return(this_bucket_ptr)
            }
        });

        loop_result
            .returned()
            .ok_or_else(|| (maybe_state.unwrap(), modifier))
    }

    fn insert_for_grow(
        &self,
        guard: &'g Guard,
        hash: u64,
        bucket_ptr: Shared<'g, Bucket<K, V>>,
    ) -> Option<usize> {
        assert!(!bucket_ptr.is_null());
        assert_eq!(bucket_ptr.tag() & SENTINEL_TAG, 0);
        assert_ne!(bucket_ptr.tag() & BORROWED_TAG, 0);

        let key = &unsafe { bucket_ptr.deref() }.key;

        let loop_result = self.probe_loop(guard, hash, |i, this_bucket, this_bucket_ptr| {
            if let Some(Bucket { key: this_key, .. }) = unsafe { this_bucket_ptr.as_ref() } {
                if this_bucket_ptr == bucket_ptr {
                    return ProbeLoopAction::Return(None);
                } else if this_key != key {
                    return ProbeLoopAction::Continue;
                } else if this_bucket_ptr.tag() & BORROWED_TAG == 0 {
                    return ProbeLoopAction::Return(None);
                }
            }

            if this_bucket_ptr.is_null() && bucket_ptr.tag() & TOMBSTONE_TAG != 0 {
                ProbeLoopAction::Return(None)
            } else if this_bucket
                .compare_and_set_weak(
                    this_bucket_ptr,
                    bucket_ptr,
                    (Ordering::Release, Ordering::Relaxed),
                    guard,
                )
                .is_ok()
            {
                ProbeLoopAction::Return(Some(i))
            } else {
                ProbeLoopAction::Reload
            }
        });

        loop_result.returned().flatten()
    }
}

impl<'g, K: 'g, V: 'g> BucketArray<K, V> {
    fn probe_loop<
        F: FnMut(usize, &Atomic<Bucket<K, V>>, Shared<'g, Bucket<K, V>>) -> ProbeLoopAction<T>,
        T,
    >(
        &self,
        guard: &'g Guard,
        hash: u64,
        mut f: F,
    ) -> ProbeLoopResult<T> {
        let offset = hash as usize & (self.buckets.len() - 1);

        for i in
            (0..self.buckets.len()).map(|i| (i.wrapping_add(offset)) & (self.buckets.len() - 1))
        {
            let this_bucket = &self.buckets[i];

            loop {
                let this_bucket_ptr = this_bucket.load_consume(guard);

                if this_bucket_ptr.tag() & SENTINEL_TAG != 0 {
                    return ProbeLoopResult::FoundSentinelTag;
                }

                match f(i, this_bucket, this_bucket_ptr) {
                    ProbeLoopAction::Continue => break,
                    ProbeLoopAction::Reload => (),
                    ProbeLoopAction::Return(t) => return ProbeLoopResult::Returned(t),
                }
            }
        }

        ProbeLoopResult::LoopEnded
    }

    pub(crate) fn rehash<H: BuildHasher>(
        &self,
        guard: &'g Guard,
        build_hasher: &H,
        rehash_op: RehashOp,
    ) -> &'g BucketArray<K, V>
    where
        K: Hash + Eq,
    {
        let next_array = self.next_array(guard, rehash_op);

        for this_bucket in self.buckets.iter() {
            let mut maybe_state: Option<(usize, Shared<'g, Bucket<K, V>>)> = None;

            loop {
                let this_bucket_ptr = this_bucket.load_consume(guard);

                if this_bucket_ptr.tag() & SENTINEL_TAG != 0 {
                    break;
                }

                let to_put_ptr = this_bucket_ptr.with_tag(this_bucket_ptr.tag() | BORROWED_TAG);

                if let Some((index, mut next_bucket_ptr)) = maybe_state {
                    assert!(!this_bucket_ptr.is_null());

                    let next_bucket = &next_array.buckets[index];

                    while next_bucket_ptr.tag() & BORROWED_TAG != 0
                        && next_bucket
                            .compare_and_set_weak(
                                next_bucket_ptr,
                                to_put_ptr,
                                (Ordering::Release, Ordering::Relaxed),
                                guard,
                            )
                            .is_err()
                    {
                        next_bucket_ptr = next_bucket.load_consume(guard);
                    }
                } else if let Some(this_bucket_ref) = unsafe { this_bucket_ptr.as_ref() } {
                    let key = &this_bucket_ref.key;
                    let hash = hash(build_hasher, key);

                    if let Some(index) = next_array.insert_for_grow(guard, hash, to_put_ptr) {
                        maybe_state = Some((index, to_put_ptr));
                    }
                }

                if this_bucket
                    .compare_and_set_weak(
                        this_bucket_ptr,
                        Shared::null().with_tag(SENTINEL_TAG),
                        (Ordering::Release, Ordering::Relaxed),
                        guard,
                    )
                    .is_ok()
                {
                    // TODO: If else, we may need to count tombstone.
                    if !this_bucket_ptr.is_null()
                        && this_bucket_ptr.tag() & TOMBSTONE_TAG != 0
                        && maybe_state.is_none()
                    {
                        unsafe { defer_destroy_bucket(guard, this_bucket_ptr) };
                    }

                    break;
                }
            }
        }

        next_array
    }

    fn next_array(&self, guard: &'g Guard, rehash_op: RehashOp) -> &'g BucketArray<K, V> {
        let mut maybe_new_next = None;

        loop {
            let next_ptr = self.next.load_consume(guard);

            if let Some(next_ref) = unsafe { next_ptr.as_ref() } {
                return next_ref;
            }

            let new_length = rehash_op.new_len(self.buckets.len());
            let new_next = maybe_new_next.unwrap_or_else(|| {
                Owned::new(BucketArray::with_length(self.epoch + 1, new_length))
            });

            match self.next.compare_and_set_weak(
                Shared::null(),
                new_next,
                (Ordering::Release, Ordering::Relaxed),
                guard,
            ) {
                Ok(p) => return unsafe { p.deref() },
                Err(CompareAndSetError { new, .. }) => {
                    maybe_new_next = Some(new);
                }
            }
        }
    }
}

#[repr(align(8))]
#[derive(Debug)]
pub(crate) struct Bucket<K, V> {
    pub(crate) key: K,
    pub(crate) maybe_value: MaybeUninit<V>,
}

impl<K, V> Bucket<K, V> {
    pub(crate) fn new(key: K, value: V) -> Bucket<K, V> {
        #[cfg(feature = "unstable-debug-counters")]
        crate::sync::debug_counters::InternalGlobalDebugCounters::bucket_created();

        Self {
            key,
            maybe_value: MaybeUninit::new(value),
        }
    }
}

#[cfg(feature = "unstable-debug-counters")]
impl<K, V> Drop for Bucket<K, V> {
    fn drop(&mut self) {
        crate::sync::debug_counters::InternalGlobalDebugCounters::bucket_dropped();
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RelocatedError;

pub(crate) enum InsertOrModifyState<K, V, F: FnOnce() -> V> {
    New(K, F),
    AttemptedInsertion(Owned<Bucket<K, V>>),
    AttemptedModification(Owned<Bucket<K, V>>, ValueOrFunction<V, F>),
}

impl<K, V, F: FnOnce() -> V> InsertOrModifyState<K, V, F> {
    fn from_bucket_value(
        bucket: Owned<Bucket<K, V>>,
        value_or_function: Option<ValueOrFunction<V, F>>,
    ) -> Self {
        if let Some(value_or_function) = value_or_function {
            Self::AttemptedModification(bucket, value_or_function)
        } else {
            Self::AttemptedInsertion(bucket)
        }
    }

    fn key(&self) -> &K {
        match self {
            InsertOrModifyState::New(k, _) => k,
            InsertOrModifyState::AttemptedInsertion(b)
            | InsertOrModifyState::AttemptedModification(b, _) => &b.key,
        }
    }

    fn into_insert_bucket(self) -> Owned<Bucket<K, V>> {
        match self {
            InsertOrModifyState::New(k, f) => Owned::new(Bucket::new(k, f())),
            InsertOrModifyState::AttemptedInsertion(b) => b,
            InsertOrModifyState::AttemptedModification(mut b, v_or_f) => {
                unsafe {
                    mem::drop(
                        mem::replace(&mut b.maybe_value, MaybeUninit::new(v_or_f.into_value()))
                            .assume_init(),
                    )
                };

                b
            }
        }
    }

    fn into_modify_bucket(self, value: V) -> (Owned<Bucket<K, V>>, ValueOrFunction<V, F>) {
        match self {
            InsertOrModifyState::New(k, f) => (
                Owned::new(Bucket::new(k, value)),
                ValueOrFunction::Function(f),
            ),
            InsertOrModifyState::AttemptedInsertion(mut b) => {
                let insert_value = unsafe {
                    mem::replace(&mut b.maybe_value, MaybeUninit::new(value)).assume_init()
                };

                (b, ValueOrFunction::Value(insert_value))
            }
            InsertOrModifyState::AttemptedModification(mut b, v_or_f) => {
                unsafe {
                    mem::drop(
                        mem::replace(&mut b.maybe_value, MaybeUninit::new(value)).assume_init(),
                    )
                };

                (b, v_or_f)
            }
        }
    }
}

pub(crate) enum ValueOrFunction<V, F: FnOnce() -> V> {
    Value(V),
    Function(F),
}

impl<V, F: FnOnce() -> V> ValueOrFunction<V, F> {
    fn into_value(self) -> V {
        match self {
            ValueOrFunction::Value(v) => v,
            ValueOrFunction::Function(f) => f(),
        }
    }
}

pub(crate) fn hash<K: ?Sized + Hash, H: BuildHasher>(build_hasher: &H, key: &K) -> u64 {
    let mut hasher = build_hasher.build_hasher();
    key.hash(&mut hasher);

    hasher.finish()
}

enum ProbeLoopAction<T> {
    Continue,
    Reload,
    Return(T),
}

enum ProbeLoopResult<T> {
    LoopEnded,
    FoundSentinelTag,
    Returned(T),
}

impl<T> ProbeLoopResult<T> {
    fn returned(self) -> Option<T> {
        match self {
            Self::Returned(t) => Some(t),
            Self::LoopEnded | Self::FoundSentinelTag => None,
        }
    }
}

pub(crate) unsafe fn defer_destroy_bucket<'g, K, V>(
    guard: &'g Guard,
    mut ptr: Shared<'g, Bucket<K, V>>,
) {
    assert!(!ptr.is_null());

    guard.defer_unchecked(move || {
        atomic::fence(Ordering::Acquire);

        if ptr.tag() & TOMBSTONE_TAG == 0 {
            ptr::drop_in_place(ptr.deref_mut().maybe_value.as_mut_ptr());
        }

        mem::drop(ptr.into_owned());
    });
}

pub(crate) unsafe fn defer_destroy_tombstone<'g, K, V>(
    guard: &'g Guard,
    mut ptr: Shared<'g, Bucket<K, V>>,
) {
    assert!(!ptr.is_null());
    assert_ne!(ptr.tag() & TOMBSTONE_TAG, 0);

    atomic::fence(Ordering::Acquire);
    // read the value now, but defer its destruction for later
    let value = ptr::read(ptr.deref_mut().maybe_value.as_ptr());

    // to be entirely honest, i don't know what order deferred functions are
    // called in crossbeam-epoch. in the case that the deferred functions are
    // called out of order, this prevents that from being an issue.
    guard.defer_unchecked(move || mem::drop(value));
}

pub(crate) unsafe fn defer_acquire_destroy<'g, T>(guard: &'g Guard, ptr: Shared<'g, T>) {
    assert!(!ptr.is_null());

    guard.defer_unchecked(move || {
        atomic::fence(Ordering::Acquire);
        mem::drop(ptr.into_owned());
    });
}

#[derive(Clone, Copy)]
pub(crate) enum RehashOp {
    Expand,
    Shrink,
    GcOnly,
    Skip,
}

impl RehashOp {
    pub(crate) fn new(cap: usize, tombstone_count: &AtomicUsize, len: &AtomicUsize) -> Self {
        let real_cap = cap as f64 * 2.0;
        let quarter_cap = real_cap / 4.0;
        let tbc = tombstone_count.load(Ordering::Relaxed) as f64;
        let len = len.load(Ordering::Relaxed) as f64;

        if tbc >= 25_000.0 || tbc / real_cap >= 0.1 {
            if len - tbc < quarter_cap && quarter_cap as usize >= BUCKET_ARRAY_DEFAULT_LENGTH {
                return Self::Shrink;
            } else {
                return Self::GcOnly;
            }
        }

        if len > real_cap * 0.7 {
            return Self::Expand;
        }

        Self::Skip
    }

    pub(crate) fn is_skip(&self) -> bool {
        matches!(self, &Self::Skip)
    }

    fn new_len(&self, current_len: usize) -> usize {
        match self {
            Self::Expand => current_len * 2,
            Self::Shrink => current_len / 2,
            Self::GcOnly => current_len,
            Self::Skip => unreachable!(),
        }
    }
}

pub(crate) const SENTINEL_TAG: usize = 0b001; // set on old table buckets when copied into a new table
pub(crate) const TOMBSTONE_TAG: usize = 0b010; // set when the value has been destroyed
pub(crate) const BORROWED_TAG: usize = 0b100; // set on new table buckets when copied from an old table

// #[cfg(test)]
// mod tests {
//     use super::*;

//     use std::collections::hash_map::RandomState;

//     #[test]
//     fn get_insert_remove() {
//         let build_hasher = RandomState::new();
//         let buckets = BucketArray::with_length(0, 16);
//         let guard = unsafe { &crossbeam_epoch::unprotected() };

//         let k1 = "foo";
//         let h1 = hash(&build_hasher, k1);
//         let v1 = 5;

//         let k2 = "bar";
//         let h2 = hash(&build_hasher, k2);
//         let v2 = 10;

//         let k3 = "baz";
//         let h3 = hash(&build_hasher, k3);
//         let v3 = 15;

//         assert_eq!(buckets.get(guard, h1, k1), Ok(Shared::null()));
//         assert_eq!(buckets.get(guard, h2, k2), Ok(Shared::null()));
//         assert_eq!(buckets.get(guard, h3, k3), Ok(Shared::null()));

//         let b1 = Owned::new(Bucket::new(k1, v1)).into_shared(guard);
//         assert!(is_ok_null(
//             buckets.insert(guard, h1, unsafe { b1.into_owned() })
//         ));

//         assert_eq!(buckets.get(guard, h1, k1), Ok(b1));
//         assert_eq!(buckets.get(guard, h2, k2), Ok(Shared::null()));
//         assert_eq!(buckets.get(guard, h3, k3), Ok(Shared::null()));

//         let b2 = Owned::new(Bucket::new(k2, v2)).into_shared(guard);
//         assert!(is_ok_null(
//             buckets.insert(guard, h2, unsafe { b2.into_owned() })
//         ));

//         assert_eq!(buckets.get(guard, h1, k1), Ok(b1));
//         assert_eq!(buckets.get(guard, h2, k2), Ok(b2));
//         assert_eq!(buckets.get(guard, h3, k3), Ok(Shared::null()));

//         let b3 = Owned::new(Bucket::new(k3, v3)).into_shared(guard);
//         assert!(is_ok_null(
//             buckets.insert(guard, h3, unsafe { b3.into_owned() })
//         ));

//         assert_eq!(buckets.get(guard, h1, k1), Ok(b1));
//         assert_eq!(buckets.get(guard, h2, k2), Ok(b2));
//         assert_eq!(buckets.get(guard, h3, k3), Ok(b3));

//         assert_eq!(
//             buckets.remove_if(guard, h1, k1, |_, _| true).ok().unwrap(),
//             b1.with_tag(TOMBSTONE_TAG)
//         );
//         unsafe { defer_destroy_tombstone(guard, b1.with_tag(TOMBSTONE_TAG)) };
//         assert_eq!(
//             buckets.remove_if(guard, h2, k2, |_, _| true).ok().unwrap(),
//             b2.with_tag(TOMBSTONE_TAG)
//         );
//         unsafe { defer_destroy_tombstone(guard, b2.with_tag(TOMBSTONE_TAG)) };
//         assert_eq!(
//             buckets.remove_if(guard, h3, k3, |_, _| true).ok().unwrap(),
//             b3.with_tag(TOMBSTONE_TAG)
//         );
//         unsafe { defer_destroy_tombstone(guard, b3.with_tag(TOMBSTONE_TAG)) };

//         assert_eq!(buckets.get(guard, h1, k1), Ok(Shared::null()));
//         assert_eq!(buckets.get(guard, h2, k2), Ok(Shared::null()));
//         assert_eq!(buckets.get(guard, h3, k3), Ok(Shared::null()));

//         for this_bucket in buckets.buckets.iter() {
//             let this_bucket_ptr = this_bucket.swap(Shared::null(), Ordering::Relaxed, guard);

//             if this_bucket_ptr.is_null() {
//                 continue;
//             }

//             unsafe {
//                 defer_destroy_bucket(guard, this_bucket_ptr);
//             }
//         }
//     }

//     fn is_ok_null<'g, K, V, E>(maybe_bucket_ptr: Result<Shared<'g, Bucket<K, V>>, E>) -> bool {
//         if let Ok(bucket_ptr) = maybe_bucket_ptr {
//             bucket_ptr.is_null()
//         } else {
//             false
//         }
//     }
// }
