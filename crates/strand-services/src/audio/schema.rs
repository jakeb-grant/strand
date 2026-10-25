//! The schema the `audio` service serves: the text its store gives
//! `Service::schema()` to replace the builtin schema's provisional stubs
//! (`record AudioDevice`, `service audio`). The same names, fields, `rw`
//! marks and actions; the real service adds nothing the language sees.
//! No field carries a level: the peak meters are the stream the
//! `spectrum` element (M4, `spectrum(AudioDevice -> source)`) will drive,
//! subscribed by its source device (docs/decisions.md, wave4-wm (audio):
//! levels reach the language through the `spectrum` element).

/// See the module docs (the text lives in `strand-services-schema`).
pub const SCHEMA: &str = strand_services_schema::AUDIO;

#[cfg(test)]
mod tests {
    use super::SCHEMA;

    /// The builtin schema's provisional declarations: the contract.
    const BUILTIN: &str = include_str!("../../../strand-compiler/src/schema/builtin.schema");

    /// The member lines of the block that starts with `head`.
    fn block<'a>(text: &'a str, head: &str) -> Vec<&'a str> {
        let start = text.find(head).unwrap_or_else(|| panic!("no `{head}`"));
        text[start..]
            .lines()
            .skip(1)
            .map(str::trim)
            .take_while(|l| *l != "}")
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    }

    /// Every member the stubs declare is declared alike, and nothing else.
    #[test]
    fn the_schema_serves_exactly_the_provisional_declarations() {
        // `video` (services-round) shares the PipeWire loop and this test.
        let video = strand_services_schema::VIDEO;
        for (schema, stub, real) in [
            (
                SCHEMA,
                "provisional record AudioDevice key id {",
                "record AudioDevice key id {",
            ),
            (
                SCHEMA,
                "provisional record AudioStream key id {",
                "record AudioStream key id {",
            ),
            (
                SCHEMA,
                "provisional record AudioMeter key device {",
                "record AudioMeter key device {",
            ),
            (SCHEMA, "provisional service audio {", "service audio {"),
            (
                video,
                "provisional record VideoStream key id {",
                "record VideoStream key id {",
            ),
            (video, "provisional service video {", "service video {"),
        ] {
            let ours = block(schema, real);
            let theirs = block(BUILTIN, stub);
            assert_eq!(ours, theirs, "{real} differs from the stub");
        }
    }

    /// The model's record has every schema field, and nothing else.
    #[test]
    fn the_model_has_exactly_the_schema_fields() {
        let d = crate::audio::AudioDevice::new(1, "n", crate::audio::Direction::Sink);
        let shown = format!("{d:?}");
        let fields: Vec<&str> = block(SCHEMA, "record AudioDevice key id {")
            .into_iter()
            .filter(|m| !m.starts_with("action "))
            .map(|m| m.split(':').next().unwrap_or(m).trim())
            .collect();
        for name in &fields {
            assert!(
                shown.contains(&format!("{name}:")),
                "AudioDevice lacks {name}"
            );
        }
        // `AudioDevice { a: …, b: … }`: as many fields as the schema has.
        assert_eq!(shown.matches(": ").count(), fields.len(), "{shown}");
    }
}
