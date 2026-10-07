//! A node's `Props` param: what the audio thread reads (channel volumes,
//! mute) and writes. Pure functions over pod bytes, so they are tested
//! without a PipeWire daemon.

use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, PropertyFlags, Value, ValueArray};
use pipewire::spa::sys;

/// What a `Props` param says about volume and mute. Fields the param does
/// not carry are `None` (a `Props` event may carry only some of them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props {
    /// `channelVolumes`: linear, one per channel.
    pub channel_volumes: Option<Vec<f32>>,
    /// `volume`: one linear master volume (used only when a node has no
    /// channel volumes).
    pub volume: Option<f32>,
    /// `mute`.
    pub mute: Option<bool>,
}

impl Props {
    /// Reads a `Props` object pod. `None` for anything else (another
    /// param, or bytes that are not a pod object).
    pub fn parse(bytes: &[u8]) -> Option<Props> {
        let (_, value) = PodDeserializer::deserialize_any_from(bytes).ok()?;
        let Value::Object(obj) = value else {
            return None;
        };
        if obj.type_ != sys::SPA_TYPE_OBJECT_Props {
            return None;
        }
        let mut out = Props::default();
        for p in obj.properties {
            match (p.key, p.value) {
                (sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                    out.channel_volumes = Some(v);
                }
                (sys::SPA_PROP_volume, Value::Float(v)) => out.volume = Some(v),
                (sys::SPA_PROP_mute, Value::Bool(m)) => out.mute = Some(m),
                _ => {}
            }
        }
        Some(out)
    }

    /// The `Props` object pod setting what is `Some` here.
    pub fn to_pod(&self) -> Result<Vec<u8>, String> {
        let mut properties = Vec::new();
        if let Some(v) = &self.channel_volumes {
            properties.push(Property {
                key: sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(v.clone())),
            });
        }
        if let Some(v) = self.volume {
            properties.push(Property {
                key: sys::SPA_PROP_volume,
                flags: PropertyFlags::empty(),
                value: Value::Float(v),
            });
        }
        if let Some(m) = self.mute {
            properties.push(Property {
                key: sys::SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(m),
            });
        }
        let value = Value::Object(Object {
            type_: sys::SPA_TYPE_OBJECT_Props,
            id: sys::SPA_PARAM_Props,
            properties,
        });
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .map(|(cursor, _)| cursor.into_inner())
            .map_err(|e| format!("cannot build a Props pod: {e:?}"))
    }
}

/// The node name in a `default` metadata value: `{ "name": "…" }`
/// (`Spa:String:JSON`). `None` when it is not that shape.
pub fn default_name(value: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    v.get("name")?.as_str().map(str::to_owned)
}

/// The `default` metadata value naming node `name`.
pub fn default_value(name: &str) -> String {
    serde_json::json!({ "name": name }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_round_trip() {
        let p = Props {
            channel_volumes: Some(vec![0.125, 0.5]),
            volume: Some(1.0),
            mute: Some(true),
        };
        let bytes = p.to_pod().unwrap();
        assert_eq!(Props::parse(&bytes), Some(p));
        let only_mute = Props {
            mute: Some(false),
            ..Props::default()
        };
        assert_eq!(Props::parse(&only_mute.to_pod().unwrap()), Some(only_mute));
    }

    #[test]
    fn props_parse_ignores_other_keys_and_rejects_other_objects() {
        // A Props object with keys we do not read (softVolumes, a params
        // struct) as a real adapter node sends it.
        let value = Value::Object(Object {
            type_: sys::SPA_TYPE_OBJECT_Props,
            id: sys::SPA_PARAM_Props,
            properties: vec![
                Property::new(
                    sys::SPA_PROP_softVolumes,
                    Value::ValueArray(ValueArray::Float(vec![1.0])),
                ),
                Property::new(
                    sys::SPA_PROP_params,
                    Value::Struct(vec![
                        Value::String("monitor.channel-volumes".into()),
                        Value::Bool(true),
                    ]),
                ),
                Property::new(sys::SPA_PROP_mute, Value::Bool(true)),
            ],
        });
        let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .unwrap()
            .0
            .into_inner();
        assert_eq!(
            Props::parse(&bytes),
            Some(Props {
                mute: Some(true),
                ..Props::default()
            })
        );
        let other = Value::Object(Object {
            type_: sys::SPA_TYPE_OBJECT_Format,
            id: sys::SPA_PARAM_Format,
            properties: vec![],
        });
        let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &other)
            .unwrap()
            .0
            .into_inner();
        assert_eq!(Props::parse(&bytes), None);
        assert_eq!(Props::parse(b"not a pod"), None);
        assert_eq!(Props::parse(&[]), None);
    }

    #[test]
    fn default_metadata_values() {
        assert_eq!(
            default_name(r#"{"name":"alsa_output.pci"}"#).as_deref(),
            Some("alsa_output.pci")
        );
        assert_eq!(default_name(r#"{ "name": "x" }"#).as_deref(), Some("x"));
        assert_eq!(default_name("not json"), None);
        assert_eq!(default_name(r#"{"other":1}"#), None);
        assert_eq!(
            default_name(&default_value("a \"quoted\" name")).as_deref(),
            Some("a \"quoted\" name")
        );
    }
}
