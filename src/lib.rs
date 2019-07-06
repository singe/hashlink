use std::{
    borrow::Borrow,
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ops::{Index, IndexMut},
    ptr,
};

use hashbrown::{hash_map, HashMap};

pub struct LinkedHashMap<K, V, S = hash_map::DefaultHashBuilder> {
    map: HashMap<*mut Node<K, V>, (), NullHasher>,
    // We need to keep any custom hash builder outside of the HashMap so we can access it alongside
    // the entry API without mutable aliasing.
    hash_builder: S,
    // Circular linked list of nodes.  If `head` is non-null, it will point to a "guard node" which
    // will never have an initialized key or value, `head.prev` will contain the last key / value in
    // the list, `head.next` will contain the first key / value in the list.
    head: *mut Node<K, V>,
    // *Singly* linked list of free nodes.  The `prev` pointers in the free list should be assumed
    // invalid.
    free: *mut Node<K, V>,
}

impl<K, V> LinkedHashMap<K, V> {
    pub fn new() -> Self {
        Self {
            hash_builder: hash_map::DefaultHashBuilder::default(),
            map: HashMap::with_hasher(NullHasher),
            head: ptr::null_mut(),
            free: ptr::null_mut(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            hash_builder: hash_map::DefaultHashBuilder::default(),
            map: HashMap::with_capacity_and_hasher(capacity, NullHasher),
            head: ptr::null_mut(),
            free: ptr::null_mut(),
        }
    }
}

impl<K, V, S> LinkedHashMap<K, V, S> {
    pub fn with_hasher(hash_builder: S) -> Self {
        Self {
            hash_builder,
            map: HashMap::with_hasher(NullHasher),
            head: ptr::null_mut(),
            free: ptr::null_mut(),
        }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hash_builder: S) -> Self {
        Self {
            hash_builder,
            map: HashMap::with_capacity_and_hasher(capacity, NullHasher),
            head: ptr::null_mut(),
            free: ptr::null_mut(),
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        self.map.reserve(additional);
    }

    pub fn shrink_to_fit(&mut self) {
        self.map.shrink_to_fit();
        unsafe { drop_free_nodes(self.free) };
        self.free = ptr::null_mut();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.map.clear();
        if !self.head.is_null() {
            unsafe {
                drop_nodes(self.head);
                (*self.head).prev = self.head;
                (*self.head).next = self.head;
            }
        }
    }

    pub fn iter(&self) -> Iter<K, V> {
        let head = if self.head.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*self.head).prev }
        };
        Iter {
            head: head,
            tail: self.head,
            remaining: self.len(),
            marker: PhantomData,
        }
    }

    pub fn iter_mut(&mut self) -> IterMut<K, V> {
        let head = if self.head.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*self.head).prev }
        };
        IterMut {
            head: head,
            tail: self.head,
            remaining: self.len(),
            marker: PhantomData,
        }
    }

    pub fn drain(&mut self) -> Drain<K, V> {
        unsafe {
            let (head, tail) = if !self.head.is_null() {
                ((*self.head).prev, (*self.head).next)
            } else {
                (ptr::null_mut(), ptr::null_mut())
            };
            let len = self.len();

            if !self.head.is_null() {
                Box::from_raw(self.head);
                self.head = ptr::null_mut();
            }

            drop_free_nodes(self.free);
            self.free = ptr::null_mut();

            self.map.clear();

            Drain {
                head: head,
                tail: tail,
                remaining: len,
                marker: PhantomData,
            }
        }
    }

    pub fn keys(&self) -> Keys<K, V> {
        Keys { inner: self.iter() }
    }

    pub fn values(&self) -> Values<K, V> {
        Values { inner: self.iter() }
    }

    pub fn values_mut(&mut self) -> ValuesMut<K, V> {
        ValuesMut {
            inner: self.iter_mut(),
        }
    }

    pub fn front(&self) -> Option<(&K, &V)> {
        if self.is_empty() {
            return None;
        }
        unsafe {
            let front = (*self.head).prev;
            Some((&*(*front).key.as_ptr(), &*(*front).value.as_ptr()))
        }
    }

    pub fn back(&mut self) -> Option<(&K, &V)> {
        if self.is_empty() {
            return None;
        }
        unsafe {
            let back = (*self.head).next;
            Some((&*(*back).key.as_ptr(), &*(*back).value.as_ptr()))
        }
    }
}

impl<K, V, S> LinkedHashMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, S> {
        match self.raw_entry_mut().from_key(&key) {
            RawEntryMut::Occupied(occupied) => Entry::Occupied(OccupiedEntry {
                key,
                raw_entry: occupied,
            }),
            RawEntryMut::Vacant(vacant) => Entry::Vacant(VacantEntry {
                key,
                raw_entry: vacant,
            }),
        }
    }

    pub fn get<Q: ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.raw_entry().from_key(k).map(|(_, v)| v)
    }

    pub fn get_key_value<Q: ?Sized>(&self, k: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.raw_entry().from_key(k)
    }

    pub fn contains_key<Q: ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.get(k).is_some()
    }

    pub fn get_mut<Q: ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        match self.raw_entry_mut().from_key(k) {
            RawEntryMut::Occupied(occupied) => Some(occupied.into_mut()),
            RawEntryMut::Vacant(_) => None,
        }
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.raw_entry_mut().from_key(&k) {
            RawEntryMut::Occupied(mut occupied) => {
                occupied.to_back();
                Some(occupied.insert(v))
            }
            RawEntryMut::Vacant(vacant) => {
                vacant.insert(k, v);
                None
            }
        }
    }

    pub fn remove<Q: ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        match self.raw_entry_mut().from_key(&k) {
            RawEntryMut::Occupied(occupied) => Some(occupied.remove()),
            RawEntryMut::Vacant(_) => None,
        }
    }

    pub fn pop_front(&mut self) -> Option<(K, V)> {
        if self.is_empty() {
            return None;
        }
        unsafe {
            let front = (*self.head).prev;
            match self
                .map
                .raw_entry_mut()
                .from_hash(hash_key(&self.hash_builder, &*(*front).key.as_ptr()), |k| {
                    (*(**k).key.as_ptr()).eq(&*(*front).key.as_ptr())
                }) {
                hash_map::RawEntryMut::Occupied(occupied) => {
                    Some(remove_node(&mut self.free, occupied.remove_entry().0))
                }
                hash_map::RawEntryMut::Vacant(_) => None,
            }
        }
    }

    pub fn pop_back(&mut self) -> Option<(K, V)> {
        if self.is_empty() {
            return None;
        }
        unsafe {
            let back = (*self.head).next;
            match self
                .map
                .raw_entry_mut()
                .from_hash(hash_key(&self.hash_builder, &*(*back).key.as_ptr()), |k| {
                    (*(**k).key.as_ptr()).eq(&*(*back).key.as_ptr())
                }) {
                hash_map::RawEntryMut::Occupied(occupied) => {
                    Some(remove_node(&mut self.free, occupied.remove_entry().0))
                }
                hash_map::RawEntryMut::Vacant(_) => None,
            }
        }
    }
}

impl<K, V, S> LinkedHashMap<K, V, S>
where
    S: BuildHasher,
{
    pub fn raw_entry(&self) -> RawEntryBuilder<'_, K, V, S> {
        RawEntryBuilder {
            hash_builder: &self.hash_builder,
            entry: self.map.raw_entry(),
        }
    }

    pub fn raw_entry_mut(&mut self) -> RawEntryBuilderMut<'_, K, V, S> {
        RawEntryBuilderMut {
            hash_builder: &self.hash_builder,
            head: &mut self.head,
            free: &mut self.free,
            entry: self.map.raw_entry_mut(),
        }
    }
}

impl<K, V, S> Drop for LinkedHashMap<K, V, S> {
    fn drop(&mut self) {
        unsafe {
            if !self.head.is_null() {
                drop_nodes(self.head);
                Box::from_raw(self.head);
            }
            drop_free_nodes(self.free);
        }
    }
}

unsafe impl<K: Send, V: Send, S: Send> Send for LinkedHashMap<K, V, S> {}
unsafe impl<K: Sync, V: Sync, S: Sync> Sync for LinkedHashMap<K, V, S> {}

impl<'a, K, V, S, Q: ?Sized> Index<&'a Q> for LinkedHashMap<K, V, S>
where
    K: Hash + Eq + Borrow<Q>,
    S: BuildHasher,
    Q: Eq + Hash,
{
    type Output = V;

    fn index(&self, index: &'a Q) -> &V {
        self.get(index).expect("no entry found for key")
    }
}

impl<'a, K, V, S, Q: ?Sized> IndexMut<&'a Q> for LinkedHashMap<K, V, S>
where
    K: Hash + Eq + Borrow<Q>,
    S: BuildHasher,
    Q: Eq + Hash,
{
    fn index_mut(&mut self, index: &'a Q) -> &mut V {
        self.get_mut(index).expect("no entry found for key")
    }
}

impl<K: Hash + Eq + Clone, V: Clone, S: BuildHasher + Clone> Clone for LinkedHashMap<K, V, S> {
    fn clone(&self) -> Self {
        let mut map = Self::with_hasher(self.hash_builder.clone());
        map.extend(self.iter().map(|(k, v)| (k.clone(), v.clone())));
        map
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Extend<(K, V)> for LinkedHashMap<K, V, S> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<'a, K, V, S> Extend<(&'a K, &'a V)> for LinkedHashMap<K, V, S>
where
    K: 'a + Hash + Eq + Copy,
    V: 'a + Copy,
    S: BuildHasher,
{
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, iter: I) {
        for (&k, &v) in iter {
            self.insert(k, v);
        }
    }
}

pub enum Entry<'a, K, V, S> {
    Occupied(OccupiedEntry<'a, K, V>),
    Vacant(VacantEntry<'a, K, V, S>),
}

impl<'a, K, V, S> Entry<'a, K, V, S> {
    pub fn or_insert(self, default: V) -> &'a mut V
    where
        K: Hash,
        S: BuildHasher,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default),
        }
    }

    pub fn or_insert_with<F: FnOnce() -> V>(self, default: F) -> &'a mut V
    where
        K: Hash,
        S: BuildHasher,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default()),
        }
    }

    pub fn key(&self) -> &K {
        match *self {
            Entry::Occupied(ref entry) => entry.key(),
            Entry::Vacant(ref entry) => entry.key(),
        }
    }

    pub fn and_modify<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut V),
    {
        match self {
            Entry::Occupied(mut entry) => {
                f(entry.get_mut());
                Entry::Occupied(entry)
            }
            Entry::Vacant(entry) => Entry::Vacant(entry),
        }
    }
}

pub struct OccupiedEntry<'a, K, V> {
    key: K,
    raw_entry: RawOccupiedEntryMut<'a, K, V>,
}

impl<'a, K, V> OccupiedEntry<'a, K, V> {
    pub fn key(&self) -> &K {
        self.raw_entry.key()
    }

    pub fn remove_entry(self) -> (K, V) {
        self.raw_entry.remove_entry()
    }

    pub fn get(&self) -> &V {
        self.raw_entry.get()
    }

    pub fn get_mut(&mut self) -> &mut V {
        self.raw_entry.get_mut()
    }

    pub fn into_mut(self) -> &'a mut V {
        self.raw_entry.into_mut()
    }

    pub fn to_back(&mut self) {
        self.raw_entry.to_back()
    }

    pub fn to_front(&mut self) {
        self.raw_entry.to_front()
    }

    pub fn insert(&mut self, value: V) -> V {
        self.raw_entry.to_back();
        self.raw_entry.insert(value)
    }

    pub fn remove(self) -> V {
        self.raw_entry.remove()
    }

    pub fn replace_entry(mut self, value: V) -> (K, V) {
        let old_key = mem::replace(self.raw_entry.key_mut(), self.key);
        let old_value = mem::replace(self.raw_entry.get_mut(), value);
        (old_key, old_value)
    }

    pub fn replace_key(mut self) -> K {
        mem::replace(self.raw_entry.key_mut(), self.key)
    }
}

pub struct VacantEntry<'a, K, V, S> {
    key: K,
    raw_entry: RawVacantEntryMut<'a, K, V, S>,
}

impl<'a, K, V, S> VacantEntry<'a, K, V, S> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn into_key(self) -> K {
        self.key
    }

    pub fn insert(self, value: V) -> &'a mut V
    where
        K: Hash,
        S: BuildHasher,
    {
        self.raw_entry.insert(self.key, value).1
    }
}

pub struct RawEntryBuilder<'a, K, V, S> {
    hash_builder: &'a S,
    entry: hash_map::RawEntryBuilder<'a, *mut Node<K, V>, (), NullHasher>,
}

impl<'a, K, V, S> RawEntryBuilder<'a, K, V, S>
where
    S: BuildHasher,
{
    pub fn from_key<Q: ?Sized>(self, k: &Q) -> Option<(&'a K, &'a V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = hash_key(self.hash_builder, k);
        self.from_key_hashed_nocheck(hash, k)
    }

    pub fn from_key_hashed_nocheck<Q: ?Sized>(self, hash: u64, k: &Q) -> Option<(&'a K, &'a V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.from_hash(hash, move |o| k.eq(o.borrow()))
    }

    pub fn from_hash(
        self,
        hash: u64,
        mut is_match: impl FnMut(&K) -> bool,
    ) -> Option<(&'a K, &'a V)> {
        let node = *self
            .entry
            .from_hash(hash, move |k| is_match(unsafe { &*(**k).key.as_ptr() }))?
            .0;

        unsafe { Some((&*(*node).key.as_ptr(), &*(*node).value.as_ptr())) }
    }
}

unsafe impl<'a, K, V, S> Send for RawEntryBuilder<'a, K, V, S>
where
    K: Send,
    V: Send,
    S: Send,
{
}

unsafe impl<'a, K, V, S> Sync for RawEntryBuilder<'a, K, V, S>
where
    K: Sync,
    V: Sync,
    S: Sync,
{
}

pub struct RawEntryBuilderMut<'a, K, V, S> {
    hash_builder: &'a S,
    head: &'a mut *mut Node<K, V>,
    free: &'a mut *mut Node<K, V>,
    entry: hash_map::RawEntryBuilderMut<'a, *mut Node<K, V>, (), NullHasher>,
}

impl<'a, K, V, S> RawEntryBuilderMut<'a, K, V, S>
where
    S: BuildHasher,
{
    pub fn from_key<Q: ?Sized>(self, k: &Q) -> RawEntryMut<'a, K, V, S>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = hash_key(self.hash_builder, k);
        self.from_key_hashed_nocheck(hash, k)
    }

    pub fn from_key_hashed_nocheck<Q: ?Sized>(self, hash: u64, k: &Q) -> RawEntryMut<'a, K, V, S>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.from_hash(hash, move |o| k.eq(o.borrow()))
    }

    pub fn from_hash(
        self,
        hash: u64,
        mut is_match: impl FnMut(&K) -> bool,
    ) -> RawEntryMut<'a, K, V, S> {
        let entry = self
            .entry
            .from_hash(hash, move |k| is_match(unsafe { &*(**k).key.as_ptr() }));

        match entry {
            hash_map::RawEntryMut::Occupied(occupied) => {
                RawEntryMut::Occupied(RawOccupiedEntryMut {
                    free: self.free,
                    head: self.head,
                    entry: occupied,
                })
            }
            hash_map::RawEntryMut::Vacant(vacant) => RawEntryMut::Vacant(RawVacantEntryMut {
                hash_builder: self.hash_builder,
                head: self.head,
                free: self.free,
                entry: vacant,
            }),
        }
    }
}

unsafe impl<'a, K, V, S> Send for RawEntryBuilderMut<'a, K, V, S>
where
    K: Send,
    V: Send,
    S: Send,
{
}

unsafe impl<'a, K, V, S> Sync for RawEntryBuilderMut<'a, K, V, S>
where
    K: Sync,
    V: Sync,
    S: Sync,
{
}

pub enum RawEntryMut<'a, K, V, S> {
    Occupied(RawOccupiedEntryMut<'a, K, V>),
    Vacant(RawVacantEntryMut<'a, K, V, S>),
}

impl<'a, K, V, S> RawEntryMut<'a, K, V, S> {
    pub fn or_insert(self, default_key: K, default_val: V) -> (&'a mut K, &'a mut V)
    where
        K: Hash,
        S: BuildHasher,
    {
        match self {
            RawEntryMut::Occupied(entry) => entry.into_key_value(),
            RawEntryMut::Vacant(entry) => entry.insert(default_key, default_val),
        }
    }

    pub fn or_insert_with<F>(self, default: F) -> (&'a mut K, &'a mut V)
    where
        F: FnOnce() -> (K, V),
        K: Hash,
        S: BuildHasher,
    {
        match self {
            RawEntryMut::Occupied(entry) => entry.into_key_value(),
            RawEntryMut::Vacant(entry) => {
                let (k, v) = default();
                entry.insert(k, v)
            }
        }
    }

    pub fn and_modify<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut K, &mut V),
    {
        match self {
            RawEntryMut::Occupied(mut entry) => {
                {
                    let (k, v) = entry.get_key_value_mut();
                    f(k, v);
                }
                RawEntryMut::Occupied(entry)
            }
            RawEntryMut::Vacant(entry) => RawEntryMut::Vacant(entry),
        }
    }
}

pub struct RawOccupiedEntryMut<'a, K, V> {
    free: &'a mut *mut Node<K, V>,
    head: &'a mut *mut Node<K, V>,
    entry: hash_map::RawOccupiedEntryMut<'a, *mut Node<K, V>, ()>,
}

impl<'a, K, V> RawOccupiedEntryMut<'a, K, V> {
    pub fn key(&self) -> &K {
        self.get_key_value().0
    }

    pub fn key_mut(&mut self) -> &mut K {
        self.get_key_value_mut().0
    }

    pub fn into_key(self) -> &'a mut K {
        self.into_key_value().0
    }

    pub fn get(&self) -> &V {
        self.get_key_value().1
    }

    pub fn get_mut(&mut self) -> &mut V {
        self.get_key_value_mut().1
    }

    pub fn into_mut(self) -> &'a mut V {
        self.into_key_value().1
    }

    pub fn get_key_value(&self) -> (&K, &V) {
        unsafe {
            let node = *self.entry.key();
            (&*(*node).key.as_ptr(), &*(*node).value.as_ptr())
        }
    }

    pub fn get_key_value_mut(&mut self) -> (&mut K, &mut V) {
        unsafe {
            let node = *self.entry.key_mut();
            (
                &mut *(*node).key.as_mut_ptr(),
                &mut *(*node).value.as_mut_ptr(),
            )
        }
    }

    pub fn into_key_value(self) -> (&'a mut K, &'a mut V) {
        unsafe {
            let node = *self.entry.into_key();
            (
                &mut *(*node).key.as_mut_ptr(),
                &mut *(*node).value.as_mut_ptr(),
            )
        }
    }

    pub fn to_back(&mut self) {
        unsafe {
            let node = *self.entry.key_mut();
            detach_node(node);
            attach_node(*self.head, node);
        }
    }

    pub fn to_front(&mut self) {
        unsafe {
            let node = *self.entry.key_mut();
            detach_node(node);
            attach_node((**self.head).prev, node);
        }
    }

    pub fn insert(&mut self, value: V) -> V {
        unsafe {
            let node = *self.entry.key_mut();
            mem::replace(&mut *(*node).value.as_mut_ptr(), value)
        }
    }

    pub fn insert_key(&mut self, key: K) -> K {
        unsafe {
            let node = *self.entry.key_mut();
            mem::replace(&mut *(*node).key.as_mut_ptr(), key)
        }
    }

    pub fn remove(self) -> V {
        self.remove_entry().1
    }

    pub fn remove_entry(self) -> (K, V) {
        let node = self.entry.remove_entry().0;
        unsafe { remove_node(self.free, node) }
    }
}

pub struct RawVacantEntryMut<'a, K, V, S> {
    hash_builder: &'a S,
    head: &'a mut *mut Node<K, V>,
    free: &'a mut *mut Node<K, V>,
    entry: hash_map::RawVacantEntryMut<'a, *mut Node<K, V>, (), NullHasher>,
}

impl<'a, K, V, S> RawVacantEntryMut<'a, K, V, S> {
    pub fn insert(self, key: K, value: V) -> (&'a mut K, &'a mut V)
    where
        K: Hash,
        S: BuildHasher,
    {
        let hash = hash_key(self.hash_builder, &key);
        self.insert_hashed_nocheck(hash, key, value)
    }

    pub fn insert_hashed_nocheck(self, hash: u64, key: K, value: V) -> (&'a mut K, &'a mut V)
    where
        K: Hash,
        S: BuildHasher,
    {
        let hash_builder = self.hash_builder;
        self.insert_with_hasher(hash, key, value, |k| hash_key(hash_builder, k))
    }

    pub fn insert_with_hasher(
        self,
        hash: u64,
        key: K,
        value: V,
        hasher: impl Fn(&K) -> u64,
    ) -> (&'a mut K, &'a mut V)
    where
        S: BuildHasher,
    {
        unsafe {
            ensure_guard_node(self.head);
            let new_node = allocate_node(self.free);
            (*new_node).key.as_mut_ptr().write(key);
            (*new_node).value.as_mut_ptr().write(value);
            attach_node(*self.head, new_node);

            let node = *self
                .entry
                .insert_with_hasher(hash, new_node, (), move |k| hasher(&*(**k).key.as_ptr()))
                .0;

            (
                &mut *(*node).key.as_mut_ptr(),
                &mut *(*node).value.as_mut_ptr(),
            )
        }
    }
}

unsafe impl<'a, K, V> Send for RawOccupiedEntryMut<'a, K, V>
where
    K: Send,
    V: Send,
{
}

unsafe impl<'a, K, V> Sync for RawOccupiedEntryMut<'a, K, V>
where
    K: Sync,
    V: Sync,
{
}

unsafe impl<'a, K, V, S> Send for RawVacantEntryMut<'a, K, V, S>
where
    K: Send,
    V: Send,
    S: Send,
{
}

unsafe impl<'a, K, V, S> Sync for RawVacantEntryMut<'a, K, V, S>
where
    K: Sync,
    V: Sync,
    S: Sync,
{
}

pub struct Iter<'a, K, V> {
    head: *const Node<K, V>,
    tail: *const Node<K, V>,
    remaining: usize,
    marker: PhantomData<(&'a K, &'a V)>,
}

pub struct IterMut<'a, K, V> {
    head: *mut Node<K, V>,
    tail: *mut Node<K, V>,
    remaining: usize,
    marker: PhantomData<(&'a K, &'a mut V)>,
}

pub struct Drain<K, V> {
    head: *mut Node<K, V>,
    tail: *mut Node<K, V>,
    remaining: usize,
    marker: PhantomData<(K, V)>,
}

unsafe impl<'a, K, V> Send for Iter<'a, K, V>
where
    K: Send,
    V: Send,
{
}
unsafe impl<'a, K, V> Send for IterMut<'a, K, V>
where
    K: Send,
    V: Send,
{
}
unsafe impl<K, V> Send for Drain<K, V>
where
    K: Send,
    V: Send,
{
}

unsafe impl<'a, K, V> Sync for Iter<'a, K, V>
where
    K: Sync,
    V: Sync,
{
}
unsafe impl<'a, K, V> Sync for IterMut<'a, K, V>
where
    K: Sync,
    V: Sync,
{
}
unsafe impl<K, V> Sync for Drain<K, V>
where
    K: Sync,
    V: Sync,
{
}

impl<'a, K, V> Clone for Iter<'a, K, V> {
    fn clone(&self) -> Self {
        Iter { ..*self }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        if self.head == self.tail {
            None
        } else {
            self.remaining -= 1;
            unsafe {
                let r = Some((&*(*self.head).key.as_ptr(), &*(*self.head).value.as_ptr()));
                self.head = (*self.head).prev;
                r
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.head == self.tail {
            None
        } else {
            self.remaining -= 1;
            unsafe {
                let r = Some((
                    &*(*self.head).key.as_ptr(),
                    &mut *(*self.head).value.as_mut_ptr(),
                ));
                self.head = (*self.head).prev;
                r
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> Iterator for Drain<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        unsafe {
            let prev = (*self.head).prev;
            let e = *Box::from_raw(self.head);
            self.head = prev;
            Some((e.key.assume_init(), e.value.assume_init()))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, K, V> DoubleEndedIterator for Iter<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> {
        if self.head == self.tail {
            None
        } else {
            self.remaining -= 1;
            unsafe {
                self.tail = (*self.tail).next;
                Some((&*(*self.tail).key.as_ptr(), &*(*self.tail).value.as_ptr()))
            }
        }
    }
}

impl<'a, K, V> DoubleEndedIterator for IterMut<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.head == self.tail {
            None
        } else {
            self.remaining -= 1;
            unsafe {
                self.tail = (*self.tail).next;
                Some((
                    &*(*self.tail).key.as_ptr(),
                    &mut *(*self.tail).value.as_mut_ptr(),
                ))
            }
        }
    }
}

impl<K, V> DoubleEndedIterator for Drain<K, V> {
    fn next_back(&mut self) -> Option<(K, V)> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        unsafe {
            let next = (*self.tail).next;
            let e = *Box::from_raw(self.tail);
            self.tail = next;
            Some((e.key.assume_init(), e.value.assume_init()))
        }
    }
}

impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {}

impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {}

impl<K, V> ExactSizeIterator for Drain<K, V> {}

impl<K, V> Drop for Drain<K, V> {
    fn drop(&mut self) {
        for _ in 0..self.remaining {
            unsafe {
                let next = (*self.tail).next;
                (*self.tail).key.as_ptr().read();
                (*self.tail).value.as_ptr().read();
                Box::from_raw(self.tail);
                self.tail = next;
            }
        }
    }
}

#[derive(Clone)]
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        self.inner.next().map(|e| e.0)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for Keys<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a K> {
        self.inner.next_back().map(|e| e.0)
    }
}

impl<'a, K, V> ExactSizeIterator for Keys<'a, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

/// An insertion-order iterator over a `LinkedHashMap`'s values.
#[derive(Clone)]
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<&'a V> {
        self.inner.next().map(|e| e.1)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for Values<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a V> {
        self.inner.next_back().map(|e| e.1)
    }
}

impl<'a, K, V> ExactSizeIterator for Values<'a, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

pub struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

impl<'a, K, V> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<&'a mut V> {
        self.inner.next().map(|e| e.1)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for ValuesMut<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a mut V> {
        self.inner.next_back().map(|e| e.1)
    }
}

impl<'a, K, V> ExactSizeIterator for ValuesMut<'a, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<'a, K: Hash + Eq, V, S: BuildHasher> IntoIterator for &'a LinkedHashMap<K, V, S> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K: Hash + Eq, V, S: BuildHasher> IntoIterator for &'a mut LinkedHashMap<K, V, S> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> IntoIterator for LinkedHashMap<K, V, S> {
    type Item = (K, V);
    type IntoIter = Drain<K, V>;

    fn into_iter(mut self) -> Drain<K, V> {
        self.drain()
    }
}

// A ZST that asserts that the inner HashMap will not do its own key hashing
struct NullHasher;

impl BuildHasher for NullHasher {
    type Hasher = Self;

    fn build_hasher(&self) -> Self {
        Self
    }
}

impl Hasher for NullHasher {
    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("inner map should not be using its built-in hasher")
    }

    fn finish(&self) -> u64 {
        unreachable!("inner map should not be using its built-in hasher")
    }
}

struct Node<K, V> {
    key: MaybeUninit<K>,
    value: MaybeUninit<V>,
    next: *mut Node<K, V>,
    prev: *mut Node<K, V>,
}

// Allocate a circular list guard node if not present.
unsafe fn ensure_guard_node<K, V>(head: &mut *mut Node<K, V>) {
    if head.is_null() {
        *head = Box::into_raw(Box::new(Node {
            key: MaybeUninit::uninit(),
            value: MaybeUninit::uninit(),
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        }));
        (**head).next = *head;
        (**head).prev = *head;
    }
}

// Attach the `to_attach` node to the existing circular list *after* `node`.
unsafe fn attach_node<K, V>(node: *mut Node<K, V>, to_attach: *mut Node<K, V>) {
    (*to_attach).next = (*node).next;
    (*to_attach).prev = node;
    (*node).next = to_attach;
    (*(*to_attach).next).prev = to_attach;
}

unsafe fn detach_node<K, V>(node: *mut Node<K, V>) {
    (*(*node).prev).next = (*node).next;
    (*(*node).next).prev = (*node).prev;
}

unsafe fn push_free<K, V>(free_list: &mut *mut Node<K, V>, node: *mut Node<K, V>) {
    (*node).next = *free_list;
    *free_list = node;
}

unsafe fn pop_free<K, V>(free_list: &mut *mut Node<K, V>) -> *mut Node<K, V> {
    if !free_list.is_null() {
        let free = *free_list;
        *free_list = (*free).next;
        free
    } else {
        ptr::null_mut()
    }
}

unsafe fn allocate_node<K, V>(free_list: &mut *mut Node<K, V>) -> *mut Node<K, V> {
    let free = pop_free(free_list);
    if free.is_null() {
        Box::into_raw(Box::new(Node {
            key: MaybeUninit::uninit(),
            value: MaybeUninit::uninit(),
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        }))
    } else {
        free
    }
}

// Head node is assumed to be the guard node and is *not* dropped.
unsafe fn drop_nodes<K, V>(head: *mut Node<K, V>) {
    let mut cur = (*head).next;
    while cur != head {
        let next = (*cur).next;
        (*cur).key.as_ptr().read();
        (*cur).value.as_ptr().read();
        Box::from_raw(cur);
        cur = next;
    }
}

// Drops all linked free nodes starting with the given node.  Free nodes are only non-circular
// singly linked, and should have uninitialized keys / values.
unsafe fn drop_free_nodes<K, V>(mut free: *mut Node<K, V>) {
    while !free.is_null() {
        let next_free = (*free).next;
        Box::from_raw(free);
        free = next_free;
    }
}

unsafe fn remove_node<K, V>(free_list: &mut *mut Node<K, V>, node: *mut Node<K, V>) -> (K, V) {
    detach_node(node);
    push_free(free_list, node);
    let key = (*node).key.as_ptr().read();
    let value = (*node).value.as_ptr().read();
    (key, value)
}

fn hash_key<S, Q>(s: &S, k: &Q) -> u64
where
    S: BuildHasher,
    Q: Hash + ?Sized,
{
    let mut hasher = s.build_hasher();
    k.hash(&mut hasher);
    hasher.finish()
}
