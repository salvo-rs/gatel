//! Bounded, lossless snapshot encoding for future host checkpoints.
//! No I/O, authorization, source admission, compilation or durability here.
//! The bytes contain configuration secrets; checksum is not authentication.
use crate::origin::OriginLocation;
use crate::{
    Fingerprint, Hasher, Node, Origin, Property, Scalar, Snapshot, SourceId, SourceKind, SourceRev,
    SourceSnapshot, Span, Value,
};

const MAGIC: &[u8; 8] = b"GATELS01";
const HEADER: usize = 40;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_STRING: usize = 1024 * 1024;
const MAX_SOURCES: usize = 256;
const MAX_NODES: usize = 65_536;
const MAX_VALUES: usize = 262_144;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limit,
    Invalid,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "snapshot codec limit exceeded",
            Self::Invalid => "invalid snapshot encoding",
        })
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

fn checksum(bytes: &[u8]) -> Fingerprint {
    let mut hash = Hasher::new();
    hash.update_str("gatel.snapshot.codec.v1").update_bytes(bytes);
    hash.finish()
}

#[derive(Default)]
struct Budget {
    nodes: usize,
    values: usize,
}
impl Budget {
    fn node(&mut self, depth: usize) -> Result<()> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(Error::Limit);
        }
        Ok(())
    }
    fn values(&mut self, count: usize) -> Result<()> {
        self.values = self.values.checked_add(count).ok_or(Error::Limit)?;
        if self.values > MAX_VALUES {
            return Err(Error::Limit);
        }
        Ok(())
    }
}

struct Writer {
    bytes: Vec<u8>,
    budget: Budget,
}
impl Writer {
    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_BYTES - self.bytes.len() {
            return Err(Error::Limit);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn tag(&mut self, tag: u8) -> Result<()> {
        self.raw(&[tag])
    }
    fn u64(&mut self, value: u64) -> Result<()> {
        self.raw(&value.to_le_bytes())
    }
    fn count(&mut self, count: usize, max: usize) -> Result<()> {
        if count > max {
            return Err(Error::Limit);
        }
        self.raw(&(count as u32).to_le_bytes())
    }
    fn string(&mut self, text: &str) -> Result<()> {
        self.count(text.len(), MAX_STRING)?;
        self.raw(text.as_bytes())
    }
    fn origin(&mut self, origin: &Origin) -> Result<()> {
        match origin.location() {
            OriginLocation::File { file, span } => {
                self.tag(0)?;
                self.string(file)?;
                self.u64(u64::try_from(span.offset).map_err(|_| Error::Limit)?)?;
                self.u64(u64::try_from(span.length).map_err(|_| Error::Limit)?)?;
                self.raw(&span.line.to_le_bytes())?;
                self.raw(&span.column.to_le_bytes())?;
            }
            OriginLocation::Api { source, pointer } => {
                self.tag(1)?;
                self.string(source)?;
                self.string(pointer)?;
            }
            OriginLocation::Synthetic { reason } => {
                self.tag(2)?;
                self.string(reason)?;
            }
        }
        self.string(origin.path())
    }
    fn value(&mut self, value: &Value) -> Result<()> {
        match &value.scalar {
            Scalar::Null => self.tag(0)?,
            Scalar::Bool(false) => self.tag(1)?,
            Scalar::Bool(true) => self.tag(2)?,
            Scalar::Int(value) => {
                self.tag(3)?;
                self.raw(&value.to_le_bytes())?;
            }
            Scalar::Float(value) => {
                self.tag(4)?;
                self.u64(value.to_bits())?;
            }
            Scalar::Str(value) => {
                self.tag(5)?;
                self.string(value)?;
            }
        }
        self.origin(&value.origin)
    }
    fn nodes(&mut self, nodes: &[Node], depth: usize) -> Result<()> {
        self.count(nodes.len(), MAX_NODES)?;
        for node in nodes {
            self.budget.node(depth)?;
            self.string(&node.name)?;
            self.origin(&node.origin)?;
            self.budget.values(node.args.len())?;
            self.count(node.args.len(), MAX_VALUES)?;
            for arg in &node.args {
                self.value(arg)?;
            }
            self.budget.values(node.props.len())?;
            self.count(node.props.len(), MAX_VALUES)?;
            for prop in &node.props {
                self.string(&prop.name)?;
                self.origin(&prop.origin)?;
                self.value(&prop.value)?;
            }
            self.nodes(&node.children, depth + 1)?;
        }
        Ok(())
    }
}

/// Canonical source order; property/argument/child order and float bits survive.
/// Encoding failure returns no partial buffer. Recursive work is depth bounded.
pub fn encode(snapshot: &Snapshot) -> Result<Vec<u8>> {
    let mut writer = Writer {
        bytes: vec![0; HEADER],
        budget: Budget::default(),
    };
    writer.count(snapshot.sources().count(), MAX_SOURCES)?;
    for source in snapshot.sources() {
        writer.tag(match source.source.kind() {
            SourceKind::File => 0,
            SourceKind::Admin => 1,
            SourceKind::Kubernetes => 2,
        })?;
        writer.string(source.source.name())?;
        writer.u64(source.rev.0)?;
        writer.raw(source.content.as_bytes())?;
        writer.nodes(&source.nodes, 1)?;
    }
    let sum = checksum(&writer.bytes[HEADER..]);
    writer.bytes[..8].copy_from_slice(MAGIC);
    writer.bytes[8..HEADER].copy_from_slice(sum.as_bytes());
    Ok(writer.bytes)
}

struct Reader<'a> {
    bytes: &'a [u8],
    budget: Budget,
}
impl<'a> Reader<'a> {
    fn raw(&mut self, count: usize) -> Result<&'a [u8]> {
        let (head, tail) = self.bytes.split_at_checked(count).ok_or(Error::Invalid)?;
        self.bytes = tail;
        Ok(head)
    }
    fn tag(&mut self) -> Result<u8> {
        Ok(self.raw(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.raw(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.raw(8)?.try_into().unwrap()))
    }
    fn count(&mut self, max: usize) -> Result<usize> {
        let count = self.u32()? as usize;
        if count > max {
            return Err(Error::Limit);
        }
        Ok(count)
    }
    fn string(&mut self) -> Result<String> {
        let count = self.count(MAX_STRING)?;
        let text = std::str::from_utf8(self.raw(count)?).map_err(|_| Error::Invalid)?;
        Ok(text.to_owned())
    }
    fn origin(&mut self) -> Result<Origin> {
        let origin = match self.tag()? {
            0 => {
                let file = self.string()?;
                let offset = usize::try_from(self.u64()?).map_err(|_| Error::Limit)?;
                let length = usize::try_from(self.u64()?).map_err(|_| Error::Limit)?;
                let line = self.u32()?;
                let column = self.u32()?;
                Origin::file(file, Span::new(offset, length, line, column))
            }
            1 => Origin::api(self.string()?, self.string()?),
            2 => Origin::synthetic(self.string()?),
            _ => return Err(Error::Invalid),
        };
        Ok(origin.field(&self.string()?))
    }
    fn value(&mut self) -> Result<Value> {
        let scalar = match self.tag()? {
            0 => Scalar::Null,
            1 => Scalar::Bool(false),
            2 => Scalar::Bool(true),
            3 => Scalar::Int(i64::from_le_bytes(self.raw(8)?.try_into().unwrap())),
            4 => Scalar::Float(f64::from_bits(self.u64()?)),
            5 => Scalar::Str(self.string()?),
            _ => return Err(Error::Invalid),
        };
        Ok(Value::new(scalar, self.origin()?))
    }
    fn nodes(&mut self, depth: usize) -> Result<Vec<Node>> {
        let count = self.count(MAX_NODES)?;
        let mut nodes = Vec::new(); // Never allocate from an untrusted count.
        for _ in 0..count {
            self.budget.node(depth)?;
            let name = self.string()?;
            let origin = self.origin()?;
            let mut node = Node::new(name, origin);
            let args = self.count(MAX_VALUES)?;
            self.budget.values(args)?;
            for _ in 0..args {
                node.args.push(self.value()?);
            }
            let props = self.count(MAX_VALUES)?;
            self.budget.values(props)?;
            for _ in 0..props {
                node.props.push(Property {
                    name: self.string()?,
                    origin: self.origin()?,
                    value: self.value()?,
                });
            }
            node.children = self.nodes(depth + 1)?;
            nodes.push(node);
        }
        Ok(nodes)
    }
}

/// Reject complete corruption, trailing bytes, duplicate sources and oversized
/// input before exposing a Snapshot. The host must separately validate identity,
/// permissions, source admission, registry compatibility and compilation.
pub fn decode(bytes: &[u8]) -> Result<Snapshot> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    if bytes.len() < HEADER
        || &bytes[..8] != MAGIC
        || checksum(&bytes[HEADER..]).as_bytes() != &bytes[8..HEADER]
    {
        return Err(Error::Invalid);
    }
    let mut reader = Reader {
        bytes: &bytes[HEADER..],
        budget: Budget::default(),
    };
    let count = reader.count(MAX_SOURCES)?;
    let mut snapshot = Snapshot::new();
    let mut previous = None;
    for _ in 0..count {
        let tag = reader.tag()?;
        let name = reader.string()?;
        let source = match tag {
            0 => SourceId::file(name),
            1 if name == SourceId::admin().name() => SourceId::admin(),
            2 => SourceId::kubernetes(name),
            _ => return Err(Error::Invalid),
        };
        if previous.as_ref().is_some_and(|previous| previous >= &source) {
            return Err(Error::Invalid);
        }
        previous = Some(source.clone());
        let rev = SourceRev(reader.u64()?);
        let content = Fingerprint::from_bytes(reader.raw(32)?.try_into().unwrap());
        snapshot.insert(SourceSnapshot::new(source, rev, content, reader.nodes(1)?));
    }
    if !reader.bytes.is_empty() {
        return Err(Error::Invalid);
    }
    Ok(snapshot)
}

#[cfg(test)]
#[path = "snapshot_codec_tests.rs"]
mod tests;
