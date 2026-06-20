#![allow(dead_code)]

use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rbx_dom_weak::{InstanceBuilder, WeakDom, ustr};
use rbx_types::{Ref, Variant};
use rbx_xml::{DecodeOptions, DecodePropertyBehavior, EncodeOptions, EncodePropertyBehavior};

use crate::{Conflict, Diagnostic, FileFormat, MergeInput, MergeOptions, MergeResult, merge};

pub fn model_path(name: &str, file_name: &str) -> PathBuf {
    test_files_root().join("models").join(name).join(file_name)
}

pub fn read_fixture(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read fixture {}", path.display()))
}

pub fn edit_fixture<F>(path: &Path, edit: F) -> Result<Vec<u8>>
where
    F: FnOnce(&mut WeakDom) -> Result<()>,
{
    let mut dom = decode_fixture(path)?;
    edit(&mut dom)?;
    encode_fixture(&dom, path)
}

pub fn decode_fixture(path: &Path) -> Result<WeakDom> {
    let bytes = read_fixture(path)?;
    decode_bytes(&bytes, path)
}

pub fn edit_bytes<F>(bytes: &[u8], path_hint: &Path, edit: F) -> Result<Vec<u8>>
where
    F: FnOnce(&mut WeakDom) -> Result<()>,
{
    let mut dom = decode_bytes(bytes, path_hint)?;
    edit(&mut dom)?;
    encode_fixture(&dom, path_hint)
}

pub fn decode_bytes(bytes: &[u8], path_hint: &Path) -> Result<WeakDom> {
    match file_format(path_hint)? {
        FileFormat::XmlModel | FileFormat::XmlPlace => {
            let options =
                DecodeOptions::new().property_behavior(DecodePropertyBehavior::ReadUnknown);
            Ok(rbx_xml::from_reader(Cursor::new(bytes), options)?)
        }
        FileFormat::BinaryModel | FileFormat::BinaryPlace => {
            Ok(rbx_binary::from_reader(Cursor::new(bytes))?)
        }
    }
}

pub fn encode_fixture(dom: &WeakDom, path_hint: &Path) -> Result<Vec<u8>> {
    let root_refs = dom.root().children();
    let mut bytes = Vec::new();

    match file_format(path_hint)? {
        FileFormat::XmlModel | FileFormat::XmlPlace => {
            let options =
                EncodeOptions::new().property_behavior(EncodePropertyBehavior::WriteUnknown);
            rbx_xml::to_writer(&mut bytes, dom, root_refs, options)?;
        }
        FileFormat::BinaryModel | FileFormat::BinaryPlace => {
            rbx_binary::to_writer(&mut bytes, dom, root_refs)?;
        }
    }

    Ok(bytes)
}

pub fn merge_fixture_bytes(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    path_hint: &Path,
    options: MergeOptions,
) -> Result<MergeResult> {
    Ok(merge(
        MergeInput {
            base,
            ours,
            theirs,
            path_hint: Some(path_hint),
        },
        options,
    )?)
}

/// Run `f` (which contains the snapshot assertion) with an insta filter that
/// rewrites absolute fixture paths to be relative to the repository root, so
/// snapshots do not embed a machine-specific prefix.
pub fn with_path_redaction<R>(f: impl FnOnce() -> R) -> R {
    let mut settings = insta::Settings::clone_current();
    settings.add_filter(r#"[^"]*/rbx-test-files/"#, "rbx-test-files/");
    settings.bind(f)
}

pub fn expect_clean(result: MergeResult) -> (Vec<u8>, Vec<Diagnostic>) {
    match result {
        MergeResult::Clean {
            merged,
            diagnostics,
        } => (merged, diagnostics),
        MergeResult::Conflicted { conflicts, .. } => {
            panic!("expected clean merge, got conflicts: {conflicts:#?}")
        }
    }
}

pub fn expect_conflicted(result: MergeResult) -> (Vec<Conflict>, Vec<Diagnostic>) {
    match result {
        MergeResult::Conflicted {
            conflicts,
            diagnostics,
        } => (conflicts, diagnostics),
        MergeResult::Clean { .. } => panic!("expected conflicted merge, got clean output"),
    }
}

pub fn xml_string(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("merged XML was not UTF-8")
}

pub fn find_by_name(dom: &WeakDom, name: &str) -> Result<Ref> {
    for instance in dom.descendants() {
        if instance.name == name {
            return Ok(instance.referent());
        }
    }

    bail!("fixture did not contain an instance named {name:?}")
}

pub fn set_property<V>(
    dom: &mut WeakDom,
    instance_name: &str,
    property_name: &str,
    value: V,
) -> Result<()>
where
    V: Into<Variant>,
{
    let referent = find_by_name(dom, instance_name)?;
    let instance = dom
        .get_by_ref_mut(referent)
        .with_context(|| format!("instance named {instance_name:?} disappeared"))?;

    instance
        .properties
        .insert(ustr(property_name), value.into());
    Ok(())
}

pub fn rename_instance(dom: &mut WeakDom, old_name: &str, new_name: &str) -> Result<()> {
    let referent = find_by_name(dom, old_name)?;
    let instance = dom
        .get_by_ref_mut(referent)
        .with_context(|| format!("instance named {old_name:?} disappeared"))?;

    instance.name = new_name.to_owned();
    Ok(())
}

pub fn insert_child(dom: &mut WeakDom, parent_name: &str, child: InstanceBuilder) -> Result<Ref> {
    let parent = find_by_name(dom, parent_name)?;
    Ok(dom.insert(parent, child))
}

pub fn insert_child_at_root(dom: &mut WeakDom, child: InstanceBuilder) -> Ref {
    let root = dom.root_ref();
    dom.insert(root, child)
}

pub fn delete_instance(dom: &mut WeakDom, name: &str) -> Result<()> {
    let referent = find_by_name(dom, name)?;
    dom.destroy(referent);
    Ok(())
}

pub fn reparent(dom: &mut WeakDom, child_name: &str, new_parent_name: &str) -> Result<()> {
    let child = find_by_name(dom, child_name)?;
    let parent = find_by_name(dom, new_parent_name)?;
    dom.transfer_within(child, parent);
    Ok(())
}

/// Move an instance to the end of its current parent's child list, which
/// reorders siblings without changing the tree shape.
pub fn move_to_end(dom: &mut WeakDom, name: &str) -> Result<()> {
    let referent = find_by_name(dom, name)?;
    let parent = dom
        .get_by_ref(referent)
        .with_context(|| format!("instance named {name:?} disappeared"))?
        .parent();
    dom.transfer_within(referent, parent);
    Ok(())
}

pub fn stable_ref(value: u128) -> Ref {
    Ref::some(value)
}

pub fn child_names(dom: &WeakDom, parent: Ref) -> Vec<String> {
    dom.get_by_ref(parent)
        .map(|instance| {
            instance
                .children()
                .iter()
                .filter_map(|child| dom.get_by_ref(*child))
                .map(|child| child.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn test_files_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .expect("rbx_merge should live under the workspace root");

    let test_files = workspace_dir.join("rbx-test-files");
    assert!(
        test_files.join("models").is_dir(),
        "could not find the rbx-test-files submodule at {}; run `git submodule update --init`",
        test_files.display()
    );
    test_files
}

fn file_format(path: &Path) -> Result<FileFormat> {
    FileFormat::from_extension(path)
        .with_context(|| format!("unknown Roblox file extension for {}", path.display()))
}
