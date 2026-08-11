use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn is_zero(n: &u32) -> bool { *n == 0 }
pub fn is_false(b: &bool) -> bool { !*b }

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    Int, Float, Bool, Str, Bytes, None,
    Seq(Box<Shape>),
    Map(Box<Shape>, Box<Shape>),
    Set(Box<Shape>),
    /// An instance of a class declared in this module, named by its class name. Produced only
    /// for a same-module class constructor call (`Foo(...)`); an imported class stays `Any`.
    Instance(String),
    Any,
    Union(Vec<Shape>),
}

impl Shape {
    pub const UNION_WIDTH_CAP: usize = 8;

    pub fn any_seq() -> Self { Shape::Seq(Box::new(Shape::Any)) }
    pub fn any_map() -> Self { Shape::Map(Box::new(Shape::Any), Box::new(Shape::Any)) }
    pub fn any_set() -> Self { Shape::Set(Box::new(Shape::Any)) }

    pub fn join(a: Shape, b: Shape) -> Shape {
        match (a, b) {
            (Shape::Any, other) | (other, Shape::Any) => other,
            (Shape::Seq(e1), Shape::Seq(e2)) => Shape::Seq(Box::new(Shape::join(*e1, *e2))),
            (Shape::Set(e1), Shape::Set(e2)) => Shape::Set(Box::new(Shape::join(*e1, *e2))),
            (Shape::Map(k1, v1), Shape::Map(k2, v2)) => Shape::Map(Box::new(Shape::join(*k1, *k2)), Box::new(Shape::join(*v1, *v2))),
            (a, b) if a == b => a,
            (a, b) => Shape::union_of([a, b]),
        }
    }

    pub fn union_of(members: impl IntoIterator<Item = Shape>) -> Shape {
        let mut flat = Vec::new();
        for m in members {
            match m {
                Shape::Union(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        if flat.iter().any(|s| matches!(s, Shape::Any)) {
            return Shape::Any;
        }
        let mut merged: Vec<Shape> = Vec::new();
        'outer: for m in flat {
            for existing in merged.iter_mut() {
                if same_constructor(existing, &m) {
                    *existing = Shape::join(existing.clone(), m);
                    continue 'outer;
                }
            }
            merged.push(m);
        }
        match merged.len() {
            0 => Shape::Any,
            1 => merged.into_iter().next().expect("len checked"),
            _ if merged.len() > Shape::UNION_WIDTH_CAP => Shape::Any,
            _ => {
                merged.sort();
                Shape::Union(merged)
            }
        }
    }
}

pub fn same_constructor(a: &Shape, b: &Shape) -> bool {
    matches!(
        (a, b),
        (Shape::Int, Shape::Int) | (Shape::Float, Shape::Float) | (Shape::Bool, Shape::Bool)
        | (Shape::Str, Shape::Str) | (Shape::Bytes, Shape::Bytes) | (Shape::None, Shape::None)
        | (Shape::Seq(_), Shape::Seq(_)) | (Shape::Map(..), Shape::Map(..)) | (Shape::Set(_), Shape::Set(_))
    ) || matches!((a, b), (Shape::Instance(x), Shape::Instance(y)) if x == y)
}

impl Serialize for Shape {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Shape::Int => serializer.serialize_str("int"),
            Shape::Float => serializer.serialize_str("float"),
            Shape::Bool => serializer.serialize_str("bool"),
            Shape::Str => serializer.serialize_str("str"),
            Shape::Bytes => serializer.serialize_str("bytes"),
            Shape::None => serializer.serialize_str("none"),
            Shape::Any => serializer.serialize_str("any"),
            Shape::Seq(elem) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("seq", elem)?;
                map.end()
            }
            Shape::Set(elem) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("set", elem)?;
                map.end()
            }
            Shape::Map(key, value) => {
                #[derive(Serialize)]
                struct MapFields<'a> {
                    key: &'a Shape,
                    value: &'a Shape,
                }
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("map", &MapFields { key, value })?;
                map.end()
            }
            Shape::Union(members) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("union", members)?;
                map.end()
            }
            Shape::Instance(name) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("instance", name)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Shape {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ShapeVisitor;
        impl<'de> Visitor<'de> for ShapeVisitor {
            type Value = Shape;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a shape tag string or a tagged shape object")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Shape, E> {
                match v {
                    "int" => Ok(Shape::Int), "float" => Ok(Shape::Float), "bool" => Ok(Shape::Bool),
                    "str" => Ok(Shape::Str), "bytes" => Ok(Shape::Bytes), "none" => Ok(Shape::None),
                    "any" => Ok(Shape::Any),
                    other => Err(de::Error::unknown_variant(other, &["int", "float", "bool", "str", "bytes", "none", "any"])),
                }
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shape, A::Error> {
                let tag: String = map.next_key()?.ok_or_else(|| de::Error::custom("expected a shape tag key"))?;
                match tag.as_str() {
                    "seq" => Ok(Shape::Seq(Box::new(map.next_value()?))),
                    "set" => Ok(Shape::Set(Box::new(map.next_value()?))),
                    "map" => {
                        #[derive(Deserialize)]
                        struct MapFields { key: Shape, value: Shape, }
                        let fields: MapFields = map.next_value()?;
                        Ok(Shape::Map(Box::new(fields.key), Box::new(fields.value)))
                    }
                    "union" => Ok(Shape::union_of(map.next_value::<Vec<Shape>>()?)),
                    "instance" => Ok(Shape::Instance(map.next_value()?)),
                    other => Err(de::Error::unknown_variant(other, &["seq", "set", "map", "union", "instance"])),
                }
            }
        }
        deserializer.deserialize_any(ShapeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_serde_round_trip() {
        let shape = Shape::Instance("Foo".to_string());
        let json = serde_json::to_string(&shape).expect("serialize");
        assert_eq!(json, r#"{"instance":"Foo"}"#);
        let back: Shape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, shape);
    }

    #[test]
    fn same_class_instance_joins_to_itself() {
        let joined = Shape::join(Shape::Instance("Foo".into()), Shape::Instance("Foo".into()));
        assert_eq!(joined, Shape::Instance("Foo".into()));
    }

    #[test]
    fn different_class_instances_join_to_a_union() {
        let joined = Shape::join(Shape::Instance("Foo".into()), Shape::Instance("Bar".into()));
        assert_eq!(
            joined,
            Shape::Union(vec![Shape::Instance("Bar".into()), Shape::Instance("Foo".into())])
        );
    }

    #[test]
    fn instance_joined_with_non_instance_shape_unions() {
        let joined = Shape::join(Shape::Instance("Foo".into()), Shape::Int);
        assert_eq!(joined, Shape::Union(vec![Shape::Int, Shape::Instance("Foo".into())]));
    }

    #[test]
    fn union_canonicalization_merges_same_class_instances() {
        let u = Shape::union_of([
            Shape::Instance("Foo".into()),
            Shape::Instance("Foo".into()),
            Shape::Instance("Bar".into()),
        ]);
        assert_eq!(
            u,
            Shape::Union(vec![Shape::Instance("Bar".into()), Shape::Instance("Foo".into())])
        );
    }
}
