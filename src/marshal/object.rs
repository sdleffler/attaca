//! `object` - the (de)serialized encoding of the Git-like data-structure

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;

use bincode;
use chrono::{DateTime, Utc};

use arc_slice::ArcSlice;
use errors::Result;
use marshal::ObjectHash;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShallowObject {
    Data(ObjectHash, u64),
    Subtree(ObjectHash),
    Commit(ObjectHash),
}


/// The marshaled, deserialized representation of a "small" object (composed of a single chunk.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawSmallObject<'a> {
    pub chunk: &'a [u8],
}


impl<'a> RawSmallObject<'a> {
    unsafe fn into_object(self, slice: ArcSlice) -> SmallObject {
        // Calculate the offset between the chunk slice's pointer and the source slice's pointer.
        let offset = slice.as_ptr().offset_to(self.chunk.as_ptr()).expect(
            "u8 is not a ZST",
        );

        assert!(offset >= 0);

        let sliced_slice = slice.map(|slice| {
            let offset = offset as usize;
            &slice[offset..offset + self.chunk.len()]
        });

        SmallObject { chunk: sliced_slice }
    }
}


#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SmallObject {
    pub chunk: ArcSlice,
}


impl SmallObject {
    pub fn size(&self) -> u64 {
        self.chunk.len() as u64
    }


    fn as_raw(&self) -> RawSmallObject {
        RawSmallObject { chunk: &self.chunk }
    }
}


/// The marshaled, deserialized representation of a "large" object (composed of smaller chunks.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LargeObject {
    pub size: u64,
    pub children: Vec<(u64, ObjectHash)>,
}


impl LargeObject {
    pub fn size(&self) -> u64 {
        self.size
    }
}


#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubtreeEntry {
    File(ObjectHash, u64),
    Subtree(ObjectHash),
}


impl SubtreeEntry {
    pub fn hash(&self) -> ObjectHash {
        match *self {
            SubtreeEntry::File(hash, _) => hash,
            SubtreeEntry::Subtree(hash) => hash,
        }
    }
}


/// The marshaled, deserialized representation of a subtree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubtreeObject {
    pub entries: BTreeMap<OsString, SubtreeEntry>,
}


/// The marshaled, deserialized representation of a commit object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitObject {
    /// The subtree the commit object references.
    pub subtree: ObjectHash,

    /// The parents of the commit.
    pub parents: Vec<ObjectHash>,

    /// A commit message, provided by the user.
    pub message: String,

    /// The commit timestamp, denoting when the commit was made locally.
    pub timestamp: DateTime<Utc>,
}


/// The marshaled, deserialized representation of a "data" object - either a small or large object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RawDataObject<'a> {
    /// A "small" blob is a single chunk.
    Small(
        #[serde(borrow)]
        RawSmallObject<'a>
    ),

    /// A "large" blob is a blob consisting of multiple smaller blobs, stored as a tuple of size
    /// and content hash.
    Large(Cow<'a, LargeObject>),
}


impl<'a> RawDataObject<'a> {
    unsafe fn into_object(self, slice: ArcSlice) -> DataObject {
        match self {
            RawDataObject::Small(small) => DataObject::Small(small.into_object(slice)),
            RawDataObject::Large(large) => DataObject::Large(large.into_owned()),
        }
    }
}


#[derive(Debug, Clone)]
pub enum DataObject {
    Small(SmallObject),
    Large(LargeObject),
}


impl DataObject {
    pub fn size(&self) -> u64 {
        match *self {
            DataObject::Small(ref small) => small.size(),
            DataObject::Large(ref large) => large.size(),
        }
    }


    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }
}


impl DataObject {
    fn as_raw(&self) -> RawDataObject {
        match *self {
            DataObject::Small(ref small) => RawDataObject::Small(small.as_raw()),
            DataObject::Large(ref large) => RawDataObject::Large(Cow::Borrowed(large)),
        }
    }
}


/// The marshaled, deserialized representation of an object in the distributed store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RawObject<'a> {
    /// A "data object" or "blob" is a single file.
    Data(
        #[serde(borrow)]
        RawDataObject<'a>
    ),

    /// A subtree is a directory, consisting of a mapping of paths to blobs.
    Subtree(Cow<'a, SubtreeObject>),

    /// A commit is a pointer to a subtree representing the current state of the repository, as
    /// well as a list of parent commits.
    Commit(Cow<'a, CommitObject>),
}


impl<'a> RawObject<'a> {
    /// Deserialize and borrow an `Object` from a byte slice.
    pub fn from_bytes(slice: &'a [u8]) -> Result<Self> {
        bincode::deserialize(slice).map_err(Into::into)
    }


    /// Serialize an `Object` into a byte vector.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serialize(self, bincode::Infinite).map_err(Into::into)
    }


    unsafe fn into_object(self, slice: ArcSlice) -> Object {
        match self {
            RawObject::Data(data) => Object::Data(data.into_object(slice)),
            RawObject::Subtree(subtree) => Object::Subtree(subtree.into_owned()),
            RawObject::Commit(commit) => Object::Commit(commit.into_owned()),
        }
    }
}


#[derive(Debug, Clone)]
pub enum Object {
    /// A "data object" or "blob" is a single file.
    Data(DataObject),

    Subtree(SubtreeObject),

    /// A commit is a pointer to a subtree representing the current state of the repository, as
    /// well as a list of parent commits.
    Commit(CommitObject),
}


impl Object {
    pub fn from_bytes(slice: ArcSlice) -> Result<Object> {
        let object = RawObject::from_bytes(&slice)?;
        Ok(unsafe { object.into_object(slice.clone()) })
    }


    pub fn as_raw(&self) -> RawObject {
        match *self {
            Object::Data(ref data) => RawObject::Data(data.as_raw()),
            Object::Subtree(ref subtree) => RawObject::Subtree(Cow::Borrowed(subtree)),
            Object::Commit(ref commit) => RawObject::Commit(Cow::Borrowed(commit)),
        }
    }


    pub fn encoded_size(&self) -> u64 {
        let raw_object = self.as_raw();
        bincode::serialized_size(&raw_object)
    }
}
