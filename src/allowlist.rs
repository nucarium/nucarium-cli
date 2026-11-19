//! The one way anything a third party hands this program is read.
//!
//! An agent's hook payload and a driver's answer both carry content beside
//! the state: prompts, titles, messages, tool input, terminal text, paths. No
//! adapter is offered a state-only source, so every adapter reads through
//! here. It deserializes into structs that name only the fields it may read,
//! each through one of the functions below, and serde skips every other key
//! unread. Nothing skipped is held, logged, spooled or sent.
//!
//! A field that holds something other than what it should, a sentence where
//! a short word belongs or an object where a number does, becomes nothing. It
//! never costs the rest of the answer, and it is never kept.
//!
//! An answer and everything nested in it is an object, and is read as one or
//! not at all. A derived struct by itself would also take an array, one value
//! after another in the order of its fields, and so read a title as a state.
//! So the top of an answer is read with [`read_object`], an object inside it
//! through [`Keyed`], [`Nested`] or [`object`], and a list of them through
//! [`rows`]. No struct read here shares its fields with another through
//! `#[serde(flatten)]`: that holds every key it does not know, and the value
//! under it, until the struct is built.

use serde::de::value::MapAccessDeserializer;
use serde::de::{self, DeserializeOwned, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;
use std::io::Read;
use std::marker::PhantomData;

/// An answer is cut off here. One this large is a tool result or a
/// transcript, and nothing this program reads is in it.
pub const MAX_PAYLOAD: u64 = 32 * 1024 * 1024;
/// No id, kind or state word is longer than this; a longer string is not one
/// of them.
pub const MAX_FIELD: usize = 4096;

/// What a field may be read as.
trait Wanted: Sized {
    fn text(_: &str) -> Option<Self> {
        None
    }
    fn truth(_: bool) -> Option<Self> {
        None
    }
    fn whole(_: u64) -> Option<Self> {
        None
    }
}

impl Wanted for String {
    fn text(value: &str) -> Option<String> {
        (value.len() <= MAX_FIELD).then(|| value.to_string())
    }
}

impl Wanted for u64 {
    fn whole(value: u64) -> Option<u64> {
        Some(value)
    }
}

impl Wanted for bool {
    fn truth(value: bool) -> Option<bool> {
        Some(value)
    }
}

/// Reads one value as `T` when it is one, and reads past it when it is not.
struct Only<T>(std::marker::PhantomData<T>);

impl<'de, T: Wanted> Visitor<'de> for Only<T> {
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("anything")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(T::text(value))
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(T::truth(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(u64::try_from(value).ok().and_then(T::whole))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(T::whole(value))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

fn only<'de, T: Wanted, D: Deserializer<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    deserializer.deserialize_any(Only(std::marker::PhantomData))
}

/// A short string: an id, a kind, a state word. Use as
/// `#[serde(default, deserialize_with = "short_string")]` on an
/// `Option<String>`. The caller still checks the word against the list its
/// third party documents: short is not the same as known.
pub fn short_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    only(deserializer)
}

/// A whole number that is not negative: a counter or a time in milliseconds.
/// For an `Option<u64>`.
pub fn number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    only(deserializer)
}

/// A yes or a no, and nothing for the word "yes". For an `Option<bool>`.
pub fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    only(deserializer)
}

/// Whether a field is there and is not null, without reading what it holds.
/// For a `bool`, with `#[serde(default, deserialize_with = "present")]`.
pub fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    struct There;

    impl<'de> Visitor<'de> for There {
        type Value = bool;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("anything")
        }

        fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_none<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
            IgnoredAny::deserialize(d).map(|_| true)
        }

        fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
            Ok(true)
        }

        fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
            Ok(true)
        }

        fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
            Ok(true)
        }

        fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
            Ok(true)
        }

        fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
            Ok(true)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(true)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(true)
        }
    }

    deserializer.deserialize_any(There)
}

// ---- words told apart from their absence ------------------------------------

/// A short string, with "not there" told apart from "there, and not a short
/// string". For a third party's own rule of the form "this field, or else
/// that one": the second is used only when the first is missing or null, and
/// never because the first held something odd. `#[serde(default)]` on the
/// field makes a missing key `Absent`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Stated {
    /// Null, or no key at all.
    #[default]
    Absent,
    /// Something that is not a short string: read past, and never kept.
    Odd,
    Word(String),
}

impl Stated {
    pub fn word(&self) -> Option<&str> {
        match self {
            Stated::Word(word) => Some(word),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Stated {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Stated, D::Error> {
        struct Said;

        impl<'de> Visitor<'de> for Said {
            type Value = Stated;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("anything")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Stated, E> {
                Ok(String::text(value).map_or(Stated::Odd, Stated::Word))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Stated, E> {
                Ok(Stated::Absent)
            }

            fn visit_none<E: de::Error>(self) -> Result<Stated, E> {
                Ok(Stated::Absent)
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Stated, D::Error> {
                d.deserialize_any(self)
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Stated, E> {
                Ok(Stated::Odd)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Stated, E> {
                Ok(Stated::Odd)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Stated, E> {
                Ok(Stated::Odd)
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Stated, E> {
                Ok(Stated::Odd)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Stated, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Stated::Odd)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Stated, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Stated::Odd)
            }
        }

        deserializer.deserialize_any(Said)
    }
}

// ---- objects, and lists of them ---------------------------------------------

/// `T`, read from a JSON object and from nothing else. Anything in its place
/// that is not an object is an error, and costs the answer it is part of:
/// for an adapter whose third party's answer is all or nothing.
#[derive(Debug)]
pub struct Keyed<T>(pub T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Keyed<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
            type Value = Keyed<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Keyed)
            }
        }

        deserializer.deserialize_any(Object(PhantomData))
    }
}

/// A value that should be an object, read as `T` when it is one. `Absent` is
/// null or no key at all, and `Odd` is anything else that is not an object:
/// read past, and never kept. One row with a sentence where an object belongs
/// then costs that row and not the whole answer. `#[serde(default)]` on the
/// field makes a missing key `Absent`.
#[derive(Debug, Default)]
pub enum Nested<T> {
    #[default]
    Absent,
    Odd,
    Is(T),
}

impl<T> Nested<T> {
    pub fn get(&self) -> Option<&T> {
        match self {
            Nested::Is(value) => Some(value),
            _ => None,
        }
    }

    pub fn into_option(self) -> Option<T> {
        match self {
            Nested::Is(value) => Some(value),
            _ => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nested<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Shape<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for Shape<T> {
            type Value = Nested<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("anything")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Nested::Is)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Nested::Absent)
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Nested::Absent)
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_any(self)
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Nested::Odd)
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Nested::Odd)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Nested::Odd)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Nested::Odd)
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Nested::Odd)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Nested::Odd)
            }
        }

        deserializer.deserialize_any(Shape(PhantomData))
    }
}

/// A named object inside an answer, as `T` when it is an object and nothing
/// when it is anything else. [`Nested`], for a field that does not need to
/// tell "not there" from "not an object": for an `Option<T>`, with
/// `#[serde(default, deserialize_with = "object")]`.
pub fn object<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Nested::<T>::deserialize(deserializer).map(Nested::into_option)
}

/// The objects of a named list, and how many of its entries were not one.
#[derive(Debug)]
pub struct Rows<T> {
    pub rows: Vec<T>,
    /// Entries that were not objects: read past, and never kept. A list with
    /// any is not a whole list, and nothing may be ended for being missing
    /// from it.
    pub skipped: usize,
}

impl<T> Default for Rows<T> {
    fn default() -> Rows<T> {
        Rows {
            rows: Vec::new(),
            skipped: 0,
        }
    }
}

/// A named list of objects, and nothing when what is there is not a list.
/// For an `Option<Rows<T>>`, with
/// `#[serde(default, deserialize_with = "rows")]`.
pub fn rows<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Rows<T>>, D::Error> {
    struct List<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for List<T> {
        type Value = Option<Rows<T>>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("anything")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut list = Rows::default();
            while let Some(row) = seq.next_element::<Nested<T>>()? {
                match row {
                    Nested::Is(row) => list.rows.push(row),
                    Nested::Absent | Nested::Odd => list.skipped += 1,
                }
            }
            Ok(Some(list))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }

        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(List(PhantomData))
}

// ---- reading an answer ------------------------------------------------------

/// Read one JSON answer from a stream into an allowlist struct, or nothing
/// when it is not one. The stream is read as it is parsed and never held
/// whole, and is cut off at [`MAX_PAYLOAD`].
///
/// `T` decides what shape is taken. An adapter reading a struct of its own
/// uses [`read_object`], so that an array is never read as one.
pub fn read<T: DeserializeOwned>(payload: &mut dyn Read) -> Option<T> {
    serde_json::from_reader(payload.take(MAX_PAYLOAD)).ok()
}

/// As [`read`], for an answer that is a JSON object: a hook's payload, a
/// driver's reply. Anything else, an array of the right values in the right
/// order among them, is nothing.
pub fn read_object<T: DeserializeOwned>(payload: &mut dyn Read) -> Option<T> {
    read::<Keyed<T>>(payload).map(|Keyed(answer)| answer)
}

/// As [`read`], for an answer that is one line of a stream that stays open:
/// a socket that answers and then waits. Reads up to the first newline or
/// `limit` bytes, whichever comes first, and parses that. The caller sets the
/// stream's own time limit; this sets none.
pub fn read_line<T: DeserializeOwned>(stream: &mut dyn Read, limit: u64) -> Option<T> {
    let mut line = Line {
        stream: stream.take(limit.min(MAX_PAYLOAD)),
        ended: false,
    };
    let answer = T::deserialize(&mut serde_json::Deserializer::from_reader(&mut line)).ok();
    // Whatever follows the answer on its line is read past, unkept, so the
    // next line starts where a line starts.
    let _ = std::io::copy(&mut line, &mut std::io::sink());
    answer
}

/// A reader that ends at the first newline, so a parser never waits on a
/// stream whose other end is not going to close.
struct Line<R> {
    stream: R,
    ended: bool,
}

impl<R: Read> Read for Line<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.ended || buffer.is_empty() {
            return Ok(0);
        }
        // One byte at a time: nothing past the newline may be taken from the
        // stream, and an answer is small.
        let mut byte = [0u8; 1];
        match self.stream.read(&mut byte)? {
            0 => {
                self.ended = true;
                Ok(0)
            }
            _ if byte[0] == b'\n' => {
                self.ended = true;
                Ok(0)
            }
            _ => {
                buffer[0] = byte[0];
                Ok(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        #[serde(default, deserialize_with = "short_string")]
        state: Option<String>,
        #[serde(default, deserialize_with = "number")]
        turn: Option<u64>,
        #[serde(default, deserialize_with = "flag")]
        awaited: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        deleted_at: bool,
    }

    fn row(value: serde_json::Value) -> Row {
        read_object(&mut value.to_string().as_bytes()).expect("a row")
    }

    #[test]
    fn a_named_field_is_read_and_every_other_field_is_skipped() {
        let read = row(json!({
            "state": "working", "turn": 7, "awaited": true, "deleted_at": "2026-11-20",
            "title": "what the agent was asked", "messages": [{ "text": "the answer" }],
        }));
        assert_eq!(
            read,
            Row {
                state: Some("working".into()),
                turn: Some(7),
                awaited: Some(true),
                deleted_at: true,
            }
        );
        assert_eq!(
            row(json!({})),
            Row {
                state: None,
                turn: None,
                awaited: None,
                deleted_at: false,
            }
        );
    }

    #[test]
    fn a_field_holding_the_wrong_kind_of_thing_is_nothing_and_costs_nothing_else() {
        let odd = row(json!({
            "state": { "word": "working", "why": ["a long explanation"] },
            "turn": "seven",
            "awaited": "yes",
            "deleted_at": null,
        }));
        assert_eq!(odd.state, None);
        assert_eq!(odd.turn, None);
        assert_eq!(odd.awaited, None);
        assert!(!odd.deleted_at);
        let more = row(json!({ "state": 4, "turn": -1, "awaited": 1, "deleted_at": { "at": 1 } }));
        assert_eq!(more.state, None);
        assert_eq!(more.turn, None);
        assert_eq!(more.awaited, None);
        assert!(more.deleted_at);
        assert_eq!(row(json!({ "turn": 1.5 })).turn, None);
    }

    #[test]
    fn a_string_too_long_to_be_a_word_is_not_kept() {
        let long = "x".repeat(MAX_FIELD + 1);
        assert_eq!(row(json!({ "state": long })).state, None);
        let fits = "x".repeat(MAX_FIELD);
        assert_eq!(row(json!({ "state": fits })).state, Some(fits));
    }

    #[test]
    fn what_is_not_json_is_nothing() {
        assert_eq!(read::<Row>(&mut "not json".as_bytes()), None);
        assert_eq!(read::<Row>(&mut "".as_bytes()), None);
        assert_eq!(read::<Row>(&mut "\"a sentence\"".as_bytes()), None);
        assert_eq!(read_object::<Row>(&mut "not json".as_bytes()), None);
        assert_eq!(read_object::<Row>(&mut "{} and more".as_bytes()), None);
    }

    #[test]
    fn an_array_is_never_read_as_the_object_it_lines_up_with() {
        // The right values in the order of the struct's fields: a derived
        // struct alone takes this, and reads whatever comes first as a state.
        let lined_up = "[\"working\", 7, true, null]";
        assert!(read::<Row>(&mut lined_up.as_bytes()).is_some());
        assert_eq!(read_object::<Row>(&mut lined_up.as_bytes()), None);
        assert_eq!(read_object::<Row>(&mut "null".as_bytes()), None);
        assert_eq!(read_object::<Row>(&mut "\"working\"".as_bytes()), None);
        assert!(read_object::<Row>(&mut "{\"state\":\"working\"}".as_bytes()).is_some());
    }

    #[derive(Debug, Deserialize)]
    struct Outer {
        #[serde(default)]
        status: Nested<Row>,
        #[serde(default, deserialize_with = "object")]
        request: Option<Row>,
        #[serde(default, deserialize_with = "rows")]
        list: Option<Rows<Row>>,
        #[serde(default)]
        run: Stated,
        #[serde(default)]
        strict: Option<Keyed<Row>>,
    }

    fn outer(value: serde_json::Value) -> Option<Outer> {
        read_object(&mut value.to_string().as_bytes())
    }

    #[test]
    fn an_object_inside_an_answer_is_an_object_or_is_told_apart_from_one() {
        let whole = outer(json!({
            "status": { "state": "idle", "title": "what it was asked" },
            "request": { "state": "working" },
        }))
        .unwrap();
        assert_eq!(whole.status.get().unwrap().state.as_deref(), Some("idle"));
        assert_eq!(whole.request.unwrap().state.as_deref(), Some("working"));
        let missing = outer(json!({ "status": null })).unwrap();
        assert!(matches!(missing.status, Nested::Absent));
        assert!(matches!(outer(json!({})).unwrap().status, Nested::Absent));
        assert!(missing.request.is_none());
        // A sentence, a number or an array where the object belongs costs
        // that object and nothing else, and an array is never lined up.
        for odd in [json!("a sentence"), json!(7), json!(["idle", 3, true, 1])] {
            let read = outer(json!({ "status": odd, "request": odd, "run": "running" })).unwrap();
            assert!(matches!(read.status, Nested::Odd), "{odd}");
            assert!(read.request.is_none(), "{odd}");
            assert_eq!(read.run.word(), Some("running"));
        }
        // Where the answer is all or nothing, it is nothing.
        let Keyed(strict) = outer(json!({ "strict": { "state": "idle" } }))
            .unwrap()
            .strict
            .unwrap();
        assert_eq!(strict.state.as_deref(), Some("idle"));
        assert!(outer(json!({ "strict": ["idle", 3, true, 1] })).is_none());
        assert!(outer(json!({ "strict": "a sentence" })).is_none());
    }

    #[test]
    fn a_list_keeps_its_objects_and_counts_what_was_not_one() {
        let list = outer(json!({
            "list": [{ "state": "idle" }, "a row that is a sentence", ["idle", 1], 7, null,
                     { "state": "working", "messages": ["the answer"] }],
        }))
        .unwrap()
        .list
        .unwrap();
        let states: Vec<_> = list.rows.iter().map(|row| row.state.as_deref()).collect();
        assert_eq!(states, [Some("idle"), Some("working")]);
        assert_eq!(list.skipped, 4);
        for not_a_list in [
            json!({ "a": 1 }),
            json!("a sentence"),
            json!(3),
            json!(null),
        ] {
            assert!(outer(json!({ "list": not_a_list })).unwrap().list.is_none());
        }
        assert!(outer(json!({})).unwrap().list.is_none());
    }

    #[test]
    fn a_word_that_is_missing_is_told_apart_from_one_that_is_not_a_word() {
        let run = |value: serde_json::Value| outer(json!({ "run": value })).unwrap().run;
        assert_eq!(run(json!("running")), Stated::Word("running".into()));
        assert_eq!(run(json!(null)), Stated::Absent);
        assert_eq!(outer(json!({})).unwrap().run, Stated::Absent);
        for odd in [
            json!(3),
            json!(true),
            json!(1.5),
            json!(["running"]),
            json!({ "is": "running" }),
        ] {
            assert_eq!(run(odd.clone()), Stated::Odd, "{odd}");
        }
        assert_eq!(run(json!("x".repeat(MAX_FIELD + 1))), Stated::Odd);
        assert_eq!(Stated::Odd.word(), None);
    }

    #[test]
    fn one_line_is_read_from_a_stream_that_stays_open() {
        // Two answers on one stream: only the first line is taken, and the
        // rest is left where it was.
        let mut stream = "{\"state\":\"idle\"}\n{\"state\":\"working\"}\n".as_bytes();
        let first: Row = read_line(&mut stream, 4096).unwrap();
        assert_eq!(first.state.as_deref(), Some("idle"));
        let second: Row = read_line(&mut stream, 4096).unwrap();
        assert_eq!(second.state.as_deref(), Some("working"));
        assert_eq!(read_line::<Row>(&mut stream, 4096), None);
        // An answer longer than its limit is no answer.
        let mut long = "{\"state\":\"idle\",\"padding\":\"xxxxxxxxxxxxxxxxxxxx\"}\n".as_bytes();
        assert_eq!(read_line::<Row>(&mut long, 16), None);
    }
}
