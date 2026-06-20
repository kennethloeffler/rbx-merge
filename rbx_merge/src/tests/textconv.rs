use anyhow::Result;

use super::common;
use crate::textconv;

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
