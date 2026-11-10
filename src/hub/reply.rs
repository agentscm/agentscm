//! How a client program reads a Hub reply.
//!
//! The Hub refuses a request that holds a field or value it does not know. A client does the
//! opposite with a reply: it ignores a field it does not know, and it reports a value it does not
//! know by name instead of taking it for a known one. A newer Hub can then add to a reply without
//! breaking a client that is already installed.
//!
//! Both directions use the same types. Their own `Deserialize` stays strict, which is what the Hub
//! applies to requests; [`decode`] is the lenient reading of the same type, for replies only.
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;

use serde::de::value::{BorrowedStrDeserializer, StrDeserializer};
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde::{Deserialize, Serialize, forward_to_deserialize_any};
use serde_json::{Map, Value};

/// The longest piece of a reply an error repeats.
const SHOWN_CHARS: usize = 80;
/// A field name no type has. Reading an object with it first makes a strict type name its fields.
const PROBE: &str = "\0agentscm-probe";
static NULL: Value = Value::Null;

/// Why a reply could not be read as the type asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyError {
    /// The reply holds a value this build does not know. `value` is the Hub's own text.
    UnknownValue { path: String, value: String },
    /// The reply does not have the shape this build reads.
    Invalid(String),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownValue { path, value } => write!(
                f,
                "the Hub sent {path} = {value:?}, which this version of agentscm does not know; update agentscm to read it"
            ),
            Self::Invalid(detail) => write!(f, "the Hub's reply could not be read: {detail}"),
        }
    }
}

impl std::error::Error for ReplyError {}

impl From<ReplyError> for crate::Error {
    fn from(error: ReplyError) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// Reads a Hub reply as `T`, ignoring fields `T` does not have.
///
/// A value `T` has no variant for is [`ReplyError::UnknownValue`], which names it; it is never
/// read as another variant. A caller that needs only some facts of a reply, and must keep working
/// when another part holds such a value, reads its own struct with [`Seen`] for the enums.
///
/// Serde reads an internally tagged enum from a private copy, where this reader cannot leave a
/// field out as it goes. An unknown field there fails the read; the reply is then read again
/// without that field, once per such field. The same holds for a flattened field. An untagged
/// enum hides which field failed, so an unknown field inside one is [`ReplyError::Invalid`].
pub fn decode<T: DeserializeOwned>(reply: &Value) -> Result<T, ReplyError> {
    let mut skipped = BTreeSet::new();
    loop {
        let fault = match read::<T>(reply, &skipped, None) {
            Ok(value) => return Ok(value),
            Err(fault) => fault,
        };
        let at = fault.at.unwrap_or_default();
        match fault.kind {
            FaultKind::Field { name, .. } => {
                let Some(holder) = holder_of::<T>(reply, &skipped, &at, &name) else {
                    return Err(ReplyError::Invalid(format!(
                        "{} holds an unknown field {:?} where this version cannot skip it",
                        shown_path(&at),
                        clip(&name)
                    )));
                };
                skipped.insert((holder, name));
            }
            FaultKind::Value(value) => {
                return Err(ReplyError::UnknownValue {
                    path: shown_path(&at),
                    value: clip(&value),
                });
            }
            FaultKind::Missing(name) => {
                return Err(ReplyError::Invalid(format!(
                    "{} lacks the field {name:?}",
                    shown_path(&at)
                )));
            }
            FaultKind::Other(detail) => {
                return Err(ReplyError::Invalid(format!(
                    "{}: {}",
                    shown_path(&at),
                    clip(&detail)
                )));
            }
        }
    }
}

/// [`decode`] for a reply still in bytes.
pub fn decode_slice<T: DeserializeOwned>(reply: &[u8]) -> Result<T, ReplyError> {
    let value =
        serde_json::from_slice(reply).map_err(|_| ReplyError::Invalid("it is not JSON".into()))?;
    decode(&value)
}

/// For a client that passes the reply on as JSON: the reply must have the shape of `T`. A value
/// this build does not know stays in the JSON the client prints, so it does not fail the check;
/// the reply after that value is not checked.
pub fn check<T: DeserializeOwned>(reply: &Value) -> Result<(), ReplyError> {
    match decode::<T>(reply) {
        Ok(_) | Err(ReplyError::UnknownValue { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// What a command prints for a reply: `text` of it, or the reply as the Hub sent it when `json`
/// was asked for or when it holds a value this build has no text for. The second part, when
/// there is one, says which value that is; it is for standard error.
pub fn shown<T: DeserializeOwned>(
    reply: &Value,
    json: bool,
    text: impl FnOnce(&T) -> String,
) -> Result<(String, Option<String>), ReplyError> {
    match decode::<T>(reply) {
        Ok(value) if !json => Ok((text(&value), None)),
        Ok(_) => Ok((reply.to_string(), None)),
        Err(newer @ ReplyError::UnknownValue { .. }) => Ok((
            reply.to_string(),
            (!json).then(|| format!("{newer}. Showing the Hub's reply as JSON.")),
        )),
        Err(error) => Err(error),
    }
}

/// Prints what [`shown`] returns.
pub fn print<T: DeserializeOwned>(
    reply: &Value,
    json: bool,
    text: impl FnOnce(&T) -> String,
) -> Result<(), ReplyError> {
    let (output, notice) = shown(reply, json, text)?;
    if let Some(notice) = notice {
        eprintln!("{notice}");
    }
    println!("{output}");
    Ok(())
}

/// An enum value in a reply: one this build knows, or the Hub's own text for one it does not.
/// Code that branches on it has to say what an unknown value means there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen<E> {
    Known(E),
    Unknown(String),
}

impl<'de, E: Deserialize<'de>> Deserialize<'de> for Seen<E> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let known = E::deserialize(StrDeserializer::<de::value::Error>::new(&text));
        Ok(known.map_or(Self::Unknown(text), Self::Known))
    }
}

/// The value as the Hub wrote it.
impl<E: Serialize> fmt::Display for Seen<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(known) => match serde_json::to_value(known) {
                Ok(Value::String(text)) => f.write_str(&text),
                Ok(other) => write!(f, "{other}"),
                Err(_) => Err(fmt::Error),
            },
            Self::Unknown(text) => f.write_str(text),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Key(String),
    Index(usize),
}

fn shown_path(path: &[Step]) -> String {
    let mut text = String::from("reply");
    for step in path {
        match step {
            Step::Key(key) => {
                text.push('.');
                text.push_str(&clip(key));
            }
            Step::Index(index) => text.push_str(&format!("[{index}]")),
        }
    }
    text
}

fn clip(text: &str) -> String {
    text.chars().take(SHOWN_CHARS).collect()
}

type Skipped = BTreeSet<(Vec<Step>, String)>;

fn read<T: DeserializeOwned>(
    reply: &Value,
    skipped: &Skipped,
    probe: Option<&[Step]>,
) -> Result<T, Fault> {
    let reader = Reader {
        path: RefCell::default(),
        skipped,
        probe,
    };
    T::deserialize(Node {
        value: reply,
        reader: &reader,
    })
}

/// The object at or below `root` whose type does not have the field `name` that failed a read.
///
/// The failure names the field, not the object. One object holding the name is the answer.
/// With several, each is read with [`PROBE`] as its first field: a strict type fails on that
/// and names its own fields, and the answer is the one whose fields lack `name`. The variant of
/// an internally tagged enum lacks its tag too, but a read without the tag says it is missing.
fn holder_of<T: DeserializeOwned>(
    reply: &Value,
    skipped: &Skipped,
    root: &[Step],
    name: &str,
) -> Option<Vec<Step>> {
    let below = root.iter().try_fold(reply, |value, step| match step {
        Step::Key(key) => value.get(key),
        Step::Index(index) => value.get(index),
    })?;
    let mut holders = Vec::new();
    find_holders(below, name, &mut root.to_vec(), &mut holders);
    holders.retain(|path| !skipped.contains(&(path.clone(), name.to_owned())));
    if holders.len() == 1 {
        return holders.pop();
    }
    holders.into_iter().find(|path| {
        let lacks = matches!(
            read::<T>(reply, skipped, Some(path.as_slice())),
            Err(Fault { kind: FaultKind::Field { name: probe, known }, .. })
                if probe == PROBE && !known.contains(&name)
        );
        lacks && {
            let mut without = skipped.clone();
            without.insert((path.clone(), name.to_owned()));
            !matches!(
                read::<T>(reply, &without, None),
                Err(Fault { kind: FaultKind::Missing(missing), .. }) if missing == name
            )
        }
    })
}

fn find_holders(value: &Value, name: &str, path: &mut Vec<Step>, found: &mut Vec<Vec<Step>>) {
    match value {
        Value::Object(map) => {
            if map.contains_key(name) {
                found.push(path.clone());
            }
            for (key, inner) in map {
                path.push(Step::Key(key.clone()));
                find_holders(inner, name, path, found);
                path.pop();
            }
        }
        Value::Array(items) => {
            for (index, inner) in items.iter().enumerate() {
                path.push(Step::Index(index));
                find_holders(inner, name, path, found);
                path.pop();
            }
        }
        _ => {}
    }
}

#[derive(Debug)]
enum FaultKind {
    /// A strict type met a field it does not have; `known` are the fields it has.
    Field {
        name: String,
        known: &'static [&'static str],
    },
    /// A type did not find a field it needs.
    Missing(&'static str),
    Value(String),
    Other(String),
}

/// A failed read, and where in the reply the nearest value this reader handed out was.
#[derive(Debug)]
struct Fault {
    kind: FaultKind,
    at: Option<Vec<Step>>,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FaultKind::Field { name, .. } => write!(f, "unknown field {name:?}"),
            FaultKind::Missing(name) => write!(f, "missing field {name:?}"),
            FaultKind::Value(value) => write!(f, "unknown value {value:?}"),
            FaultKind::Other(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for Fault {}

impl de::Error for Fault {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self {
            kind: FaultKind::Other(message.to_string()),
            at: None,
        }
    }

    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        Self {
            kind: FaultKind::Field {
                name: field.to_owned(),
                known: expected,
            },
            at: None,
        }
    }

    fn missing_field(field: &'static str) -> Self {
        Self {
            kind: FaultKind::Missing(field),
            at: None,
        }
    }

    fn unknown_variant(variant: &str, _expected: &'static [&'static str]) -> Self {
        Self {
            kind: FaultKind::Value(variant.to_owned()),
            at: None,
        }
    }
}

enum Live<'de> {
    Key(&'de str),
    Index(usize),
}

struct Reader<'de, 's> {
    path: RefCell<Vec<Live<'de>>>,
    /// Fields an earlier read failed on, by the object that holds them.
    skipped: &'s Skipped,
    /// The object to read with [`PROBE`] as its first field.
    probe: Option<&'s [Step]>,
}

impl<'de> Reader<'de, '_> {
    fn here(&self) -> Vec<Step> {
        self.path
            .borrow()
            .iter()
            .map(|step| match step {
                Live::Key(key) => Step::Key((*key).to_owned()),
                Live::Index(index) => Step::Index(*index),
            })
            .collect()
    }

    /// Reads one value below the current one, and marks a failure with where it was.
    fn under<T>(
        &self,
        step: Live<'de>,
        read: impl FnOnce() -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        self.path.borrow_mut().push(step);
        let result = read().map_err(|mut fault| {
            fault.at.get_or_insert_with(|| self.here());
            fault
        });
        self.path.borrow_mut().pop();
        result
    }
}

#[derive(Clone, Copy)]
struct Node<'de, 'r, 's> {
    value: &'de Value,
    reader: &'r Reader<'de, 's>,
}

impl<'de> Node<'de, '_, '_> {
    /// `known` is the field list of the struct being read, when serde gives one.
    fn fields<V: Visitor<'de>>(
        self,
        map: &'de Map<String, Value>,
        known: Option<&'static [&'static str]>,
        visitor: V,
    ) -> Result<V::Value, Fault> {
        let reader = self.reader;
        let (skipped, probe) = if reader.skipped.is_empty() && reader.probe.is_none() {
            (Vec::new(), false)
        } else {
            let here = reader.here();
            let skipped = reader.skipped.iter();
            (
                skipped
                    .filter(|(path, _)| *path == here)
                    .map(|(_, name)| name.as_str())
                    .collect(),
                reader.probe == Some(here.as_slice()),
            )
        };
        let mut fields = Fields {
            entries: map.iter(),
            value: None,
            known,
            skipped,
            probe,
            reader,
        };
        let value = visitor.visit_map(&mut fields)?;
        if fields.next_field().is_some() {
            return Err(de::Error::invalid_length(map.len(), &"fewer fields"));
        }
        Ok(value)
    }

    fn items<V: Visitor<'de>>(self, items: &'de [Value], visitor: V) -> Result<V::Value, Fault> {
        let mut rest = Items {
            items: items.iter().enumerate(),
            reader: self.reader,
        };
        let value = visitor.visit_seq(&mut rest)?;
        if rest.items.next().is_some() {
            return Err(de::Error::invalid_length(items.len(), &"fewer items"));
        }
        Ok(value)
    }
}

impl<'de> Deserializer<'de> for Node<'de, '_, '_> {
    type Error = Fault;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
        match self.value {
            Value::Null => visitor.visit_unit(),
            Value::Bool(value) => visitor.visit_bool(*value),
            Value::Number(number) => {
                if let Some(value) = number.as_u64() {
                    visitor.visit_u64(value)
                } else if let Some(value) = number.as_i64() {
                    visitor.visit_i64(value)
                } else if let Some(value) = number.as_f64() {
                    visitor.visit_f64(value)
                } else {
                    Err(de::Error::custom("a number is out of range"))
                }
            }
            Value::String(text) => visitor.visit_borrowed_str(text),
            Value::Array(items) => self.items(items, visitor),
            Value::Object(map) => self.fields(map, None, visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
        match self.value {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Fault> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fault> {
        match self.value {
            Value::Object(map) => self.fields(map, Some(fields), visitor),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fault> {
        match self.value {
            Value::String(name) => visitor.visit_enum(BorrowedStrDeserializer::new(name)),
            Value::Object(map) if map.len() == 1 => {
                let (name, value) = map.iter().next().expect("one entry");
                visitor.visit_enum(Variant {
                    name,
                    content: Node {
                        value,
                        reader: self.reader,
                    },
                })
            }
            _ => Err(de::Error::custom(
                "expected a text value or an object with one field",
            )),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
        unit_struct seq tuple tuple_struct map identifier
    }
}

struct Fields<'de, 'r, 's> {
    entries: serde_json::map::Iter<'de>,
    value: Option<(&'de str, &'de Value)>,
    known: Option<&'static [&'static str]>,
    skipped: Vec<&'s str>,
    probe: bool,
    reader: &'r Reader<'de, 's>,
}

impl<'de> Fields<'de, '_, '_> {
    /// The next field the type being read has; the others are the newer Hub's.
    fn next_field(&mut self) -> Option<(&'de str, &'de Value)> {
        self.entries
            .by_ref()
            .map(|(name, value)| (name.as_str(), value))
            .find(|(name, _)| {
                self.known.is_none_or(|known| known.contains(name)) && !self.skipped.contains(name)
            })
    }
}

impl<'de> MapAccess<'de> for &mut Fields<'de, '_, '_> {
    type Error = Fault;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Fault> {
        let entry = if std::mem::take(&mut self.probe) {
            Some((PROBE, &NULL))
        } else {
            self.next_field()
        };
        let Some((name, value)) = entry else {
            return Ok(None);
        };
        self.value = Some((name, value));
        seed.deserialize(Key(name)).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, Fault> {
        let (name, value) = self
            .value
            .take()
            .ok_or_else(|| de::Error::custom("a value was read before its field name"))?;
        let reader = self.reader;
        reader.under(Live::Key(name), || seed.deserialize(Node { value, reader }))
    }
}

struct Items<'de, 'r, 's> {
    items: std::iter::Enumerate<std::slice::Iter<'de, Value>>,
    reader: &'r Reader<'de, 's>,
}

impl<'de> SeqAccess<'de> for &mut Items<'de, '_, '_> {
    type Error = Fault;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Fault> {
        let Some((index, value)) = self.items.next() else {
            return Ok(None);
        };
        let reader = self.reader;
        reader
            .under(Live::Index(index), || {
                seed.deserialize(Node { value, reader })
            })
            .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

/// An externally tagged enum value: `{"name": content}`.
struct Variant<'de, 'r, 's> {
    name: &'de str,
    content: Node<'de, 'r, 's>,
}

impl<'de> EnumAccess<'de> for Variant<'de, '_, '_> {
    type Error = Fault;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self), Fault> {
        let variant = seed.deserialize(BorrowedStrDeserializer::<Fault>::new(self.name))?;
        Ok((variant, self))
    }
}

impl<'de> VariantAccess<'de> for Variant<'de, '_, '_> {
    type Error = Fault;

    fn unit_variant(self) -> Result<(), Fault> {
        self.under(<()>::deserialize)
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, Fault> {
        self.under(|content| seed.deserialize(content))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Fault> {
        self.under(|content| content.deserialize_seq(visitor))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fault> {
        self.under(|content| content.deserialize_struct("", fields, visitor))
    }
}

impl<'de, 'r, 's> Variant<'de, 'r, 's> {
    fn under<T>(
        self,
        read: impl FnOnce(Node<'de, 'r, 's>) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        let content = self.content;
        content.reader.under(Live::Key(self.name), || read(content))
    }
}

/// An object's field name, read as serde_json reads one: text, or the number or boolean it spells.
struct Key<'de>(&'de str);

macro_rules! spelled_key {
    ($($method:ident => $visit:ident,)*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
            match self.0.parse() {
                Ok(value) => visitor.$visit(value),
                Err(_) => visitor.visit_borrowed_str(self.0),
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for Key<'de> {
    type Error = Fault;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
        visitor.visit_borrowed_str(self.0)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fault> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Fault> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fault> {
        visitor.visit_enum(BorrowedStrDeserializer::new(self.0))
    }

    spelled_key! {
        deserialize_bool => visit_bool,
        deserialize_i8 => visit_i8,
        deserialize_i16 => visit_i16,
        deserialize_i32 => visit_i32,
        deserialize_i64 => visit_i64,
        deserialize_i128 => visit_i128,
        deserialize_u8 => visit_u8,
        deserialize_u16 => visit_u16,
        deserialize_u32 => visit_u32,
        deserialize_u64 => visit_u64,
        deserialize_u128 => visit_u128,
        deserialize_f32 => visit_f32,
        deserialize_f64 => visit_f64,
    }

    forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::hub::work::{WorkSource, WorkStart, WorkState};

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Inner {
        id: u8,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Colour {
        Red,
        DarkBlue,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum Tagged {
        Plain { id: u8 },
        Holding { inner: Inner },
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Carrying {
        Unit,
        One(Inner),
        Pair(u8, u8),
        Named { inner: Inner },
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Outer {
        name: String,
        count: i64,
        ratio: f64,
        colour: Colour,
        inner: Option<Inner>,
        rows: Vec<Inner>,
        tagged: Vec<Tagged>,
        carrying: Vec<Carrying>,
        by_number: BTreeMap<u8, Inner>,
        #[serde(default)]
        later: Option<String>,
        raw: Value,
    }

    fn sample() -> Value {
        json!({
            "name": "n", "count": -3, "ratio": 0.5, "colour": "dark_blue",
            "inner": {"id": 1},
            "rows": [{"id": 2}, {"id": 3}],
            "tagged": [{"kind": "plain", "id": 4}, {"kind": "holding", "inner": {"id": 5}}],
            "carrying": ["unit", {"one": {"id": 6}}, {"pair": [7, 8]}, {"named": {"inner": {"id": 9}}}],
            "by_number": {"10": {"id": 10}},
            "raw": {"anything": [1, {"id": "kept"}]},
        })
    }

    /// Every object a struct is read from gains a field no type has. `raw` is kept whole.
    fn with_new_fields(value: &mut Value) {
        match value {
            Value::Object(map) => {
                // How serde writes an enum value that carries data: not a struct.
                let variant = map.len() == 1
                    && ["one", "pair", "named"]
                        .iter()
                        .any(|name| map.contains_key(*name));
                for (name, inner) in map.iter_mut() {
                    match (name.as_str(), inner) {
                        ("raw", _) => {}
                        // A map keyed by number, not a struct: only its values gain a field.
                        ("by_number", Value::Object(rows)) => {
                            rows.values_mut().for_each(with_new_fields);
                        }
                        (_, inner) => with_new_fields(inner),
                    }
                }
                if !variant {
                    map.insert("added_later".into(), json!({"id": [1, 2]}));
                }
            }
            Value::Array(items) => items.iter_mut().for_each(with_new_fields),
            _ => {}
        }
    }

    #[test]
    fn reads_what_serde_json_reads() {
        let strict: Outer = serde_json::from_value(sample()).unwrap();
        assert_eq!(decode::<Outer>(&sample()).unwrap(), strict);
        assert_eq!(
            decode_slice::<Outer>(sample().to_string().as_bytes()).unwrap(),
            strict
        );
    }

    #[test]
    fn ignores_new_fields_at_every_depth_it_reads() {
        let mut newer = sample();
        with_new_fields(&mut newer);
        assert!(serde_json::from_value::<Outer>(newer.clone()).is_err());
        let strict: Outer = serde_json::from_value(sample()).unwrap();
        assert_eq!(decode::<Outer>(&newer).unwrap(), strict);
    }

    /// Inside a tagged value the failure names only the field. A field a type has is kept even
    /// when a type nested in it, or holding it, lacks a field of the same name.
    #[test]
    fn inside_a_tagged_value_only_the_type_without_the_field_loses_it() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct Leaf {
            id: u8,
            #[serde(default)]
            inner: Option<u8>,
        }
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Nested {
            Holding {
                #[serde(default)]
                id: Option<u8>,
                inner: Leaf,
                rows: Vec<Leaf>,
            },
        }
        // `kind` is the outer tag and a new field of each inner object. `rows` is a field of the
        // outer value and a new field of the first row. `new` is new once.
        let newer = json!({
            "kind": "holding", "id": 1,
            "inner": {"id": 2, "inner": 3, "kind": "x"},
            "rows": [
                {"id": 4, "kind": "x", "rows": [{"id": 9}]},
                {"id": 5, "inner": 6, "kind": "x", "new": {"id": 0}},
            ],
        });
        assert!(serde_json::from_value::<Nested>(newer.clone()).is_err());
        assert_eq!(
            decode::<Nested>(&newer).unwrap(),
            Nested::Holding {
                id: Some(1),
                inner: Leaf {
                    id: 2,
                    inner: Some(3)
                },
                rows: vec![
                    Leaf { id: 4, inner: None },
                    Leaf {
                        id: 5,
                        inner: Some(6)
                    }
                ],
            }
        );
    }

    #[test]
    fn an_unknown_field_inside_an_untagged_value_is_refused() {
        #[derive(Debug, Deserialize)]
        #[serde(untagged)]
        enum Either {
            #[allow(dead_code)]
            Row(Inner),
        }
        decode::<Either>(&json!({"id": 1})).unwrap();
        assert!(matches!(
            decode::<Either>(&json!({"id": 1, "added_later": 2})),
            Err(ReplyError::Invalid(_))
        ));
    }

    #[test]
    fn names_an_unknown_value_and_where_it_is() {
        let mut newer = sample();
        newer["colour"] = json!("ultraviolet");
        assert_eq!(
            decode::<Outer>(&newer).unwrap_err(),
            ReplyError::UnknownValue {
                path: "reply.colour".into(),
                value: "ultraviolet".into()
            }
        );
        let mut newer = sample();
        newer["tagged"][0] = json!({"kind": "brand_new", "id": 4});
        assert_eq!(
            decode::<Outer>(&newer).unwrap_err(),
            ReplyError::UnknownValue {
                path: "reply.tagged[0].kind".into(),
                value: "brand_new".into()
            }
        );
        let mut newer = sample();
        newer["carrying"][1] = json!({"two": {"id": 6}});
        assert_eq!(
            decode::<Outer>(&newer).unwrap_err(),
            ReplyError::UnknownValue {
                path: "reply.carrying[1]".into(),
                value: "two".into()
            }
        );
        assert_eq!(check::<Outer>(&newer), Ok(()));
        let text = decode::<Outer>(&newer).unwrap_err().to_string();
        assert!(text.contains(r#"reply.carrying[1] = "two""#), "{text}");
    }

    #[test]
    fn a_wrong_shape_is_still_refused() {
        let mut wrong = sample();
        wrong["count"] = json!("three");
        let error = decode::<Outer>(&wrong).unwrap_err();
        assert!(
            matches!(&error, ReplyError::Invalid(detail) if detail.starts_with("reply.count:"))
        );
        assert_eq!(check::<Outer>(&wrong), Err(error));
        let mut missing = sample();
        missing.as_object_mut().unwrap().remove("name");
        assert!(matches!(
            decode::<Outer>(&missing),
            Err(ReplyError::Invalid(_))
        ));
        assert!(matches!(
            decode_slice::<Outer>(b"{"),
            Err(ReplyError::Invalid(_))
        ));
        let long = "x".repeat(500);
        let ReplyError::UnknownValue { value, .. } = decode::<Colour>(&json!(long)).unwrap_err()
        else {
            panic!("expected an unknown value");
        };
        assert_eq!(value.len(), SHOWN_CHARS);
    }

    #[test]
    fn a_command_shows_text_or_the_reply_itself_with_a_notice() {
        let text = |colour: &Colour| format!("{colour:?}");
        let known = json!("red");
        assert_eq!(shown(&known, false, text), Ok(("Red".into(), None)));
        assert_eq!(shown(&known, true, text), Ok((r#""red""#.into(), None)));
        let newer = json!("ultraviolet");
        let (output, notice) = shown(&newer, false, text).unwrap();
        assert_eq!(output, r#""ultraviolet""#);
        let notice = notice.unwrap();
        assert!(notice.contains(r#"reply = "ultraviolet""#), "{notice}");
        assert!(
            notice.ends_with("Showing the Hub's reply as JSON."),
            "{notice}"
        );
        assert_eq!(
            shown(&newer, true, text),
            Ok((r#""ultraviolet""#.into(), None))
        );
        assert!(matches!(
            shown(&json!(7), false, text),
            Err(ReplyError::Invalid(_))
        ));
    }

    #[test]
    fn seen_keeps_the_hubs_text_for_a_value_it_does_not_know() {
        #[derive(Deserialize)]
        struct Row {
            state: Seen<WorkState>,
        }
        let known: Row = decode(&json!({"state": "paused", "more": [1]})).unwrap();
        assert_eq!(known.state, Seen::Known(WorkState::Paused));
        assert_eq!(known.state.to_string(), "paused");
        let newer: Row = decode(&json!({"state": "snoozed"})).unwrap();
        assert_eq!(newer.state, Seen::Unknown("snoozed".into()));
        assert_eq!(newer.state.to_string(), "snoozed");
        assert!(decode::<Row>(&json!({"state": 7})).is_err());
    }

    /// The same type, both directions: the Hub's own reading refuses what a client's ignores.
    #[test]
    fn the_hub_still_refuses_a_request_with_an_unknown_field_or_value() {
        let request = json!({
            "title": "t", "session": "s", "scope": ["src/a.rs"],
            "request_key": "k", "issue_choice": {"kind": "none_applies", "reason": "r"},
        });
        serde_json::from_value::<WorkStart>(request.clone()).unwrap();
        let mut newer = request.clone();
        newer["added_later"] = json!(1);
        newer["issue_choice"]["added_later"] = json!(1);
        let refusal = serde_json::from_value::<WorkStart>(newer.clone()).unwrap_err();
        assert!(refusal.to_string().contains("unknown field"), "{refusal}");
        decode::<WorkStart>(&newer).unwrap();
        let mut unknown = request;
        unknown["issue_choice"] = json!({"kind": "brand_new"});
        let refusal = serde_json::from_value::<WorkStart>(unknown.clone()).unwrap_err();
        assert!(refusal.to_string().contains("unknown variant"), "{refusal}");
        assert!(matches!(
            decode::<WorkStart>(&unknown),
            Err(ReplyError::UnknownValue { .. })
        ));
        let source = json!({"kind": "native", "request_key": "k", "added_later": true});
        assert!(serde_json::from_value::<WorkSource>(source.clone()).is_err());
        assert_eq!(
            decode::<WorkSource>(&source).unwrap(),
            WorkSource::Native {
                request_key: "k".into()
            }
        );
    }
}
