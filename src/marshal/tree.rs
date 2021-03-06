use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsString, OsStr};
use std::iter::FromIterator;
use std::ops::{Index, IndexMut};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::future;
use futures::prelude::*;
use futures::stream;

use errors::Error;
use marshal::{ObjectHash, SubtreeObject, SubtreeEntry, Marshaller};
use trace::Trace;


#[derive(Debug, Clone, Copy)]
pub struct NodeId(usize);


#[derive(Debug, Clone)]
pub enum Node {
    Opaque(SubtreeEntry),
    Transparent(HashMap<OsString, NodeId>),
}


#[derive(Debug, Clone)]
pub struct Arena {
    entries: Vec<Option<Node>>,
}


impl Index<NodeId> for Arena {
    type Output = Option<Node>;

    fn index(&self, node_id: NodeId) -> &Self::Output {
        &self.entries[node_id.0]
    }
}


impl IndexMut<NodeId> for Arena {
    fn index_mut(&mut self, node_id: NodeId) -> &mut Self::Output {
        &mut self.entries[node_id.0]
    }
}


impl Arena {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn alloc(&mut self, node_opt: Option<Node>) -> NodeId {
        let node_id = NodeId(self.entries.len());
        self.entries.push(node_opt);
        node_id
    }
}


#[derive(Clone)]
pub struct OccupiedEntry<I: Iterator>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    node_id: NodeId,

    tree: Tree,
    empty_iter: I,
}


impl<I: Iterator> OccupiedEntry<I>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    pub fn remove(self) -> VacantEntry<I> {
        let OccupiedEntry {
            node_id,
            mut tree,
            empty_iter,
        } = self;
        assert!(tree.arena[node_id].take().is_some());
        VacantEntry {
            node_id,
            immediate_component_opt: None,
            remaining_path: empty_iter,
            tree,
        }
    }

    pub fn replace(mut self, entry: SubtreeEntry) -> Self {
        self.tree.arena[self.node_id] = Some(Node::Opaque(entry));
        self
    }

    pub fn into_inner(self) -> Tree {
        self.tree
    }
}


#[derive(Clone)]
pub struct VacantEntry<I: Iterator>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    node_id: NodeId,

    immediate_component_opt: Option<I::Item>,
    remaining_path: I,

    tree: Tree,
}


impl<I: Iterator> VacantEntry<I>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    pub fn insert(self, entry: SubtreeEntry) -> OccupiedEntry<I> {
        let VacantEntry {
            node_id,
            immediate_component_opt,
            mut remaining_path,
            mut tree,
        } = self;

        let mut current = match immediate_component_opt {
            Some(immediate_component) => {
                let immediate_node_id = tree.arena.alloc(None);

                match tree.arena[node_id] {
                    Some(Node::Transparent(ref mut entries)) => {
                        entries.insert(immediate_component.into(), immediate_node_id);
                    }
                    Some(_) => panic!("Entry isn't actually vacant!"),
                    ref mut empty => {
                        let mut immediate_subtree = HashMap::new();
                        immediate_subtree.insert(immediate_component.into(), immediate_node_id);
                        *empty = Some(Node::Transparent(immediate_subtree));
                    }
                }

                immediate_node_id
            }
            None => node_id,
        };

        while let Some(immediate_path) = remaining_path.next() {
            let immediate_node_id = tree.arena.alloc(None);
            let mut immediate_subtree = HashMap::new();
            immediate_subtree.insert(immediate_path.into(), immediate_node_id);
            tree.arena[current] = Some(Node::Transparent(immediate_subtree));
            current = immediate_node_id;
        }
        tree.arena[current] = Some(Node::Opaque(entry));

        OccupiedEntry {
            node_id: current,
            tree,
            empty_iter: remaining_path,
        }
    }

    pub fn into_inner(self) -> Tree {
        self.tree
    }
}


pub enum Entry<I: Iterator>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    Occupied(OccupiedEntry<I>),
    Vacant(VacantEntry<I>),
}


pub struct Blocked<I: Iterator>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    blocking_id: NodeId,

    immediate_component: I::Item,
    remaining_path: I,

    tree: Tree,
}


impl<I: Iterator> Blocked<I>
where
    I::Item: Borrow<OsStr> + Into<OsString>,
{
    pub fn unblock(self, subtree: Tree) -> Result<Entry<I>, Blocked<I>> {
        let Blocked {
            blocking_id,
            immediate_component,
            remaining_path,
            mut tree,
        } = self;
        let subtree_root = tree.append(subtree);
        tree.arena[blocking_id] = tree.arena[subtree_root].take();
        tree.entry_from(blocking_id, Some(immediate_component), remaining_path)
    }

    pub fn object_hash(&self) -> ObjectHash {
        match self.tree.arena[self.blocking_id] {
            Some(Node::Opaque(ref entry)) => entry.hash(),
            _ => panic!("Tree traversal not actually blocked!"),
        }
    }

    pub fn into_inner(self) -> Tree {
        self.tree
    }
}


// A depth-first iteration over a Tree.
#[derive(Debug, Clone)]
pub struct IntoIter {
    stack: Vec<(PathBuf, NodeId)>,
    arena: Arena,
}


impl Iterator for IntoIter {
    type Item = (PathBuf, SubtreeEntry);

    fn next(&mut self) -> Option<Self::Item> {
        self.stack.pop().and_then(|(path, node_id)| {
            let node = self.arena[node_id].take().unwrap();
            match node {
                Node::Opaque(entry) => Some((path, entry)),
                Node::Transparent(entries) => {
                    self.stack.extend(
                        entries.into_iter().map(|(component, node_id)| {
                            (path.join(component), node_id)
                        }),
                    );

                    self.next()
                }
            }
        })
    }
}


#[derive(Debug, Clone)]
pub struct Tree {
    arena: Arena,
    root: NodeId,
}


impl From<HashMap<OsString, SubtreeEntry>> for Tree {
    fn from(subtree: HashMap<OsString, SubtreeEntry>) -> Tree {
        let mut arena = Arena::new();
        let new_subtree = subtree
            .into_iter()
            .map(|(key, value)| (key, arena.alloc(Some(Node::Opaque(value)))))
            .collect();
        let root = arena.alloc(Some(Node::Transparent(new_subtree)));

        Self { arena, root }
    }
}


impl From<BTreeMap<OsString, SubtreeEntry>> for Tree {
    fn from(subtree: BTreeMap<OsString, SubtreeEntry>) -> Tree {
        let mut arena = Arena::new();
        let new_subtree = subtree
            .into_iter()
            .map(|(key, value)| (key, arena.alloc(Some(Node::Opaque(value)))))
            .collect();
        let root = arena.alloc(Some(Node::Transparent(new_subtree)));

        Self { arena, root }
    }
}


impl From<SubtreeEntry> for Tree {
    fn from(root_hash: SubtreeEntry) -> Self {
        let mut arena = Arena::new();
        let root = arena.alloc(Some(Node::Opaque(root_hash)));

        Self { arena, root }
    }
}


impl FromIterator<(PathBuf, SubtreeEntry)> for Tree {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (PathBuf, SubtreeEntry)>,
    {
        let mut to_insert = iter.into_iter().collect::<Vec<_>>();
        to_insert.sort_unstable_by(|&(ref left, _), &(ref right, _)| left.cmp(right));

        let mut tree = Tree::from(BTreeMap::new());
        for (key, value) in to_insert {
            tree = match tree.entry(key.iter().map(OsStr::to_owned)) {
                Ok(Entry::Vacant(vacant)) => vacant.insert(value).into_inner(),
                Ok(Entry::Occupied(occupied)) => occupied.replace(value).into_inner(),
                Err(_) => panic!("Insertion blocked: bad subtree!"),
            };
        }

        tree
    }
}


impl IntoIterator for Tree {
    type IntoIter = IntoIter;
    type Item = (PathBuf, SubtreeEntry);

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![(PathBuf::new(), self.root)],
            arena: self.arena,
        }
    }
}


impl Tree {
    fn append(&mut self, subtree: Tree) -> NodeId {
        let offset = self.arena.entries.len();

        self.arena.entries.extend(
            subtree.arena.entries.into_iter().map(
                |mut entry| {
                    match entry {
                        Some(Node::Transparent(ref mut entries)) => {
                            entries.values_mut().for_each(|node_id| node_id.0 += offset)
                        }
                        _ => {}
                    }

                    entry
                },
            ),
        );

        NodeId(subtree.root.0 + offset)
    }

    pub fn entry<I: IntoIterator>(self, path: I) -> Result<Entry<I::IntoIter>, Blocked<I::IntoIter>>
    where
        I::Item: Borrow<OsStr> + Into<OsString>,
    {
        let root = self.root;
        self.entry_from(root, None, path.into_iter())
    }

    fn entry_from<I: Iterator>(
        self,
        mut current: NodeId,
        pre_chain: Option<I::Item>,
        mut path: I,
    ) -> Result<Entry<I>, Blocked<I>>
    where
        I::Item: Borrow<OsStr> + Into<OsString>,
    {
        enum Preliminary<T> {
            Blocked(T),
            Occupied,
            Vacant(Option<T>),
        }

        let preliminary = {
            let mut iter = pre_chain.into_iter().chain(path.by_ref());
            loop {
                match iter.next() {
                    Some(immediate_component) => {
                        match self.arena[current] {
                            Some(Node::Opaque(_)) => {
                                break Preliminary::Blocked(immediate_component)
                            }
                            Some(Node::Transparent(ref entries)) => {
                                match entries.get(immediate_component.borrow()).cloned() {
                                    Some(node_id) => current = node_id,
                                    None => break Preliminary::Vacant(Some(immediate_component)),
                                }
                            }
                            None => break Preliminary::Vacant(None),
                        }
                    }
                    None => break Preliminary::Occupied,
                }
            }
        };

        match preliminary {
            Preliminary::Blocked(immediate_component) => {
                Err(Blocked {
                    blocking_id: current,
                    immediate_component,
                    remaining_path: path,
                    tree: self,
                })
            }
            Preliminary::Occupied => {
                Ok(Entry::Occupied(OccupiedEntry {
                    node_id: current,
                    empty_iter: path,
                    tree: self,
                }))
            }
            Preliminary::Vacant(immediate_component_opt) => {
                return Ok(Entry::Vacant(VacantEntry {
                    node_id: current,
                    immediate_component_opt,
                    remaining_path: path,
                    tree: self,
                }))
            }
        }
    }

    // Boxed due to polymorphic recursion.
    // Unfortunately #[async(boxed)] does not work here for some... reason? Thinks things aren't
    // Send.
    fn marshal_inner<T: Trace>(
        this: Arc<Mutex<Arena>>,
        node_id: NodeId,
        marshaller: Marshaller<T>,
    ) -> Box<Future<Item = SubtreeEntry, Error = Error> + Send> {
        Box::new(async_block! {
            let node = this.lock().unwrap()[node_id].take().unwrap();
            match node {
                Node::Opaque(subtree_entry) => await!(marshaller.process(subtree_entry.hash()).map(|_| subtree_entry)),
                Node::Transparent(entries) => {
                    let captured_marshaller = marshaller.clone();
                    let future_entries = entries.into_iter().map(move |(key, node_id)| {
                        Self::marshal_inner(this.clone(), node_id, captured_marshaller.clone())
                            .map(|node_hash| (key, node_hash))
                    });
                    let future_node_hash =
                        stream::futures_unordered(future_entries)
                            .fold(BTreeMap::new(), |mut map, (key, hash)| {
                                map.insert(key, hash);
                                future::ok::<_, Error>(map)
                            })
                            .and_then(move |entries| marshaller.process(SubtreeObject { entries })).map(SubtreeEntry::Subtree);
                    await!(future_node_hash)
                }
            }
        })
    }

    #[async]
    pub fn marshal<T: Trace>(self, marshaller: Marshaller<T>) -> Result<ObjectHash, Error> {
        let Self { arena, root } = self;
        let entry = await!(Self::marshal_inner(
            Arc::new(Mutex::new(arena)),
            root,
            marshaller,
        ))?;

        Ok(entry.hash())
    }
}


#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashSet;

    use quickcheck::TestResult;

    quickcheck! {
        // Vec<Vec<String>> is a workaround for Vec<PathBuf>, since PathBuf has no Arbitrary and
        // neither does OsString, so Vec<Vec<OsString>> is Right Out.
        #[test]
        fn from_and_into_iter(objects: Vec<Vec<String>>) -> TestResult {
            let mut paths = objects.into_iter().map(|p| p.into_iter().collect::<PathBuf>()).collect::<Vec<_>>();

            // Only leaves are allowed here.
            paths.sort();
            for window in paths.windows(2) {
                if window[1].starts_with(&window[0]) {
                    return TestResult::discard();
                }
            }

            let tree = paths.iter().cloned().map(|path| (path, SubtreeEntry::File(ObjectHash::zero(), 0))).collect::<Tree>();
            let pre_hashset = paths.into_iter().collect::<HashSet<_>>();
            let post_hashset = tree.into_iter().map(|(path, _)| path).collect::<HashSet<_>>();

            if pre_hashset == post_hashset {
                TestResult::passed()
            } else {
                TestResult::error(format!("Mismatch: {:?}", pre_hashset.symmetric_difference(&post_hashset).collect::<Vec<_>>()))
            }
        }
    }
}
