use anyhow::Result;

use super::common;
use crate::{textconv, textconv_to};

/// The streaming renderer must produce exactly the bytes the buffered one does,
/// for every value type and tree shape in the corpus — it is the same output,
/// just flushed node by node.
#[test]
fn streaming_matches_buffered() -> Result<()> {
    for path in common::all_fixture_paths() {
        let bytes = common::read_fixture(&path)?;
        let buffered = textconv(&bytes, Some(&path))?;
        let mut streamed = Vec::new();
        textconv_to(&bytes, Some(&path), &mut streamed)?;
        assert_eq!(
            buffered.as_bytes(),
            streamed.as_slice(),
            "streamed output diverged for {}",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn textconv_snapshots_xml_model() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path))?;

    insta::assert_snapshot!("three_intvalues_xml_textconv", text);
    Ok(())
}

#[test]
fn textconv_snapshots_internal_refs() -> Result<()> {
    let path = common::model_path("ref-child", "xml.rbxmx");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path))?;

    insta::assert_snapshot!("ref_child_xml_textconv", text);
    Ok(())
}

#[test]
fn textconv_snapshots_binary_model() -> Result<()> {
    let path = common::model_path("attributes", "binary.rbxm");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path))?;

    insta::assert_snapshot!("attributes_binary_textconv", text);
    Ok(())
}
