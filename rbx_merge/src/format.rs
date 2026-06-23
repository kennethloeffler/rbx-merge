//! Roblox file-format detection and binary/XML decode/encode.

use std::{error::Error as StdError, fmt, io::Cursor, path::Path};

use rbx_dom_weak::WeakDom;
use rbx_types::Ref;
use rbx_xml::{DecodeOptions, DecodePropertyBehavior, EncodeOptions, EncodePropertyBehavior};

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileFormat {
    BinaryModel,
    BinaryPlace,
    XmlModel,
    XmlPlace,
}

impl FileFormat {
    pub fn from_extension(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("rbxm") => Some(Self::BinaryModel),
            Some("rbxl") => Some(Self::BinaryPlace),
            Some("rbxmx") => Some(Self::XmlModel),
            Some("rbxlx") => Some(Self::XmlPlace),
            _ => None,
        }
    }

    fn is_xml(self) -> bool {
        matches!(self, Self::XmlModel | Self::XmlPlace)
    }
}

impl fmt::Display for FileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FileFormat::BinaryModel => "binary model (.rbxm)",
            FileFormat::BinaryPlace => "binary place (.rbxl)",
            FileFormat::XmlModel => "XML model (.rbxmx)",
            FileFormat::XmlPlace => "XML place (.rbxlx)",
        })
    }
}

pub fn detect_format(
    bytes: &[u8],
    path_hint: Option<&Path>,
    explicit: Option<FileFormat>,
) -> Result<FileFormat, Error> {
    if let Some(format) = explicit {
        return Ok(format);
    }

    if let Some(format) = path_hint.and_then(FileFormat::from_extension) {
        return Ok(format);
    }

    if bytes.starts_with(b"<roblox!") {
        return Ok(FileFormat::BinaryModel);
    }

    let trimmed = trim_ascii_whitespace_start(bytes);
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<roblox") {
        return Ok(FileFormat::XmlModel);
    }

    Err(Error::UnknownFormat {
        path: path_hint
            .map(|path| format!(" for {}", path.display()))
            .unwrap_or_default(),
    })
}

fn trim_ascii_whitespace_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if !first.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

pub(crate) struct DecodedFile {
    pub(crate) format: FileFormat,
    pub(crate) dom: WeakDom,
}

pub(crate) fn decode_file(
    bytes: &[u8],
    path_hint: Option<&Path>,
    explicit: Option<FileFormat>,
) -> Result<DecodedFile, Error> {
    let format = detect_format(bytes, path_hint, explicit)?;
    let dom = if format.is_xml() {
        let options = DecodeOptions::new().property_behavior(DecodePropertyBehavior::ReadUnknown);
        rbx_xml::from_reader(Cursor::new(bytes), options).map_err(|source| Error::Decode {
            format,
            message: error_to_string(source),
        })?
    } else {
        rbx_binary::from_reader(Cursor::new(bytes)).map_err(|source| Error::Decode {
            format,
            message: error_to_string(source),
        })?
    };

    Ok(DecodedFile { format, dom })
}

pub(crate) fn encode_file(
    dom: &WeakDom,
    root_refs: &[Ref],
    format: FileFormat,
) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    if format.is_xml() {
        let options = EncodeOptions::new().property_behavior(EncodePropertyBehavior::WriteUnknown);
        rbx_xml::to_writer(&mut output, dom, root_refs, options).map_err(|source| {
            Error::Encode {
                format,
                message: error_to_string(source),
            }
        })?;
    } else {
        rbx_binary::to_writer(&mut output, dom, root_refs).map_err(|source| Error::Encode {
            format,
            message: error_to_string(source),
        })?;
    }
    Ok(output)
}

fn error_to_string(error: impl StdError) -> String {
    error.to_string()
}
