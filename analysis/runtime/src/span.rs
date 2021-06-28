use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::sync::RwLock;

lazy_static! {
    static ref SPAN_FILE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);
}

pub fn set_file(file_path: &str) {
    *SPAN_FILE_PATH.write().unwrap() = Some(PathBuf::from(file_path));
}

lazy_static! {
    pub(crate) static ref SOURCE_SPANS: Vec<SourceSpan> = {
        let path = SPAN_FILE_PATH.read().expect("SPAN_FILE_PATH was locked").clone()
            .expect("SPAN_FILE_PATH not initialized by the instrumented code");
        let file = File::open(&path)
            .expect(&format!("Could not open span file: {:?}", path.to_str()));
        bincode::deserialize_from(file)
            .expect("Error deserializing span file")
    };
}

pub fn get(index: SpanId) -> Option<&'static SourceSpan> {
    if SPAN_FILE_PATH.read().unwrap().is_some() {
        Some(&SOURCE_SPANS[index as usize])
    } else {
        None
    }
}

pub type SpanId = u32;

/// A byte offset. Keep this small (currently 32-bits), as AST contains
/// a lot of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct SourcePos(pub u32);

impl SourcePos {
    #[inline(always)]
    pub fn to_u32(&self) -> u32 {
        self.0
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub source: PathBuf,
    pub lo: SourcePos,
    pub hi: SourcePos,
}

impl SourceSpan {
    pub fn new(source: PathBuf, lo: SourcePos, hi: SourcePos) -> Self {
        Self {
            source,
            lo,
            hi,
        }
    }
}

impl fmt::Debug for SourceSpan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}-{}", self.source.to_str().unwrap(), self.lo.0, self.hi.0)
    }
}
