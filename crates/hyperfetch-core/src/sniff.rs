//! A file's type from its first bytes, for a download whose name does not tell it.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Bytes read from a file's start to tell its type.
const HEAD: u64 = 4096;

/// Extensions of the scripts that make a file on a server, which say nothing of the file.
const PAGE_SCRIPTS: [&str; 5] = ["php", "asp", "aspx", "jsp", "cgi"];

/// How a file of each type starts, and its extension.
const MAGIC: &[(&[u8], &str)] = &[
    (b"7z\xBC\xAF\x27\x1C", "7z"),
    (b"Rar!\x1A\x07", "rar"),
    (b"\x1F\x8B", "gz"),
    (b"\xFD7zXZ\x00", "xz"),
    (b"BZh", "bz2"),
    (b"\x28\xB5\x2F\xFD", "zst"),
    (b"%PDF-", "pdf"),
    (b"\x89PNG\r\n\x1A\n", "png"),
    (b"\xFF\xD8\xFF", "jpg"),
    (b"GIF87a", "gif"),
    (b"GIF89a", "gif"),
    (b"ID3", "mp3"),
    (b"fLaC", "flac"),
    (b"OggS", "ogg"),
    (b"!<arch>\ndebian-binary", "deb"),
    (b"\xED\xAB\xEE\xDB", "rpm"),
    (b"\x7FELF", "elf"),
    (b"MZ", "exe"),
];

/// The extension of a file that starts with `head`, from its magic bytes, if it is a type this
/// knows: archives and packages, documents, pictures, audio and video, programs. An EPUB is told
/// from another ZIP by its `mimetype` entry, when that is in `head`.
pub fn extension_for(head: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    if at(0, b"PK\x03\x04") {
        // An EPUB's first entry is `mimetype`, stored, holding its type.
        return Some(if at(30, b"mimetypeapplication/epub+zip") { "epub" } else { "zip" });
    }
    if at(4, b"ftyp") {
        return Some(match head.get(8..12)? {
            b"qt  " => "mov",
            b"M4A " | b"M4B " => "m4a",
            b"avif" | b"avis" => "avif",
            b"heic" | b"heix" | b"mif1" | b"msf1" => "heic",
            _ => "mp4",
        });
    }
    if at(0, b"\x1A\x45\xDF\xA3") {
        // Matroska's EBML header names its DocType near its start.
        let header = &head[..head.len().min(64)];
        return Some(if header.windows(4).any(|w| w == b"webm") { "webm" } else { "mkv" });
    }
    if at(0, b"RIFF") {
        return match head.get(8..12)? {
            b"WEBP" => Some("webp"),
            b"WAVE" => Some("wav"),
            b"AVI " => Some("avi"),
            _ => None,
        };
    }
    MAGIC.iter().find(|(magic, _)| head.starts_with(magic)).map(|&(_, ext)| ext)
}

/// The type [`extension_for`] reads from the start of the file at `path`. Blocking.
pub fn type_of(path: &Path) -> Option<&'static str> {
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(HEAD).read_to_end(&mut head).ok()?;
    extension_for(&head)
}

/// Whether the name `path` says nothing of the file's type: it has no extension, or a page
/// script's (`.php`, `.asp`, `.aspx`, `.jsp`, `.cgi`).
pub fn says_nothing(path: &Path) -> bool {
    path.extension().is_none_or(|ext| PAGE_SCRIPTS.iter().any(|script| ext.eq_ignore_ascii_case(script)))
}

/// `name` for the finished download at `file` (its `.part`): with the extension its contents show
/// added, when `name` says nothing of its type (see [`says_nothing`]). A real extension is never
/// replaced, and a program for Linux keeps its bare name, as programs there go. Blocking.
pub(crate) fn named_by_content(file: &Path, name: PathBuf) -> PathBuf {
    if !says_nothing(&name) {
        return name;
    }
    match (type_of(file), name.file_name()) {
        (Some(ext), Some(base)) if ext != "elf" => {
            let mut base = base.to_owned();
            base.push(format!(".{ext}"));
            name.with_file_name(base)
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_type_is_told_by_its_first_bytes() {
        let ftyp = |brand: &[u8]| [&[0, 0, 0, 0x20][..], b"ftyp", brand, b"\0\0\0\0isom"].concat();
        let riff = |kind: &[u8]| [&b"RIFF\x24\0\0\0"[..], kind].concat();
        let ebml = |doctype: &[u8]| [&b"\x1A\x45\xDF\xA3\xA3\x42\x86\x81\x01\x42\x82\x88"[..], doctype].concat();
        let epub = [&b"PK\x03\x04\x0a\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x08\0\0\0"[..], b"mimetypeapplication/epub+zip"].concat();
        for (head, ext) in [
            (b"PK\x03\x04\x14\0\0\0".to_vec(), "zip"),
            (epub, "epub"),
            (b"7z\xBC\xAF\x27\x1C\0\x04".to_vec(), "7z"),
            (b"Rar!\x1A\x07\x01\0".to_vec(), "rar"),
            (b"Rar!\x1A\x07\0".to_vec(), "rar"),
            (b"\x1F\x8B\x08\0".to_vec(), "gz"),
            (b"\xFD7zXZ\0\0\x04".to_vec(), "xz"),
            (b"BZh91AY".to_vec(), "bz2"),
            (b"\x28\xB5\x2F\xFD\x04".to_vec(), "zst"),
            (b"%PDF-1.7\n".to_vec(), "pdf"),
            (b"\x89PNG\r\n\x1A\n\0\0".to_vec(), "png"),
            (b"\xFF\xD8\xFF\xE0\0\x10JFIF".to_vec(), "jpg"),
            (b"GIF89a\x01\0".to_vec(), "gif"),
            (riff(b"WEBPVP8 "), "webp"),
            (riff(b"WAVEfmt "), "wav"),
            (riff(b"AVI LIST"), "avi"),
            (ftyp(b"isom"), "mp4"),
            (ftyp(b"mp42"), "mp4"),
            (ftyp(b"qt  "), "mov"),
            (ftyp(b"M4A "), "m4a"),
            (ftyp(b"heic"), "heic"),
            (ebml(b"matroska"), "mkv"),
            (ebml(b"webm"), "webm"),
            (b"ID3\x04\0\0".to_vec(), "mp3"),
            (b"fLaC\0\0\0\x22".to_vec(), "flac"),
            (b"OggS\0\x02".to_vec(), "ogg"),
            (b"MZ\x90\0\x03".to_vec(), "exe"),
            (b"\x7FELF\x02\x01\x01".to_vec(), "elf"),
            (b"!<arch>\ndebian-binary   1342943816  0     0     100644  4         `\n".to_vec(), "deb"),
            (b"\xED\xAB\xEE\xDB\x03\0".to_vec(), "rpm"),
        ] {
            assert_eq!(extension_for(&head), Some(ext), "{head:?}");
        }
        for unknown in [&b""[..], b"PK", b"hello world", b"<!DOCTYPE html>", b"RIFF\0\0\0\0XXXX", b"\0\0\0\x20ftyp", b"!<arch>\nlib.o/"] {
            assert_eq!(extension_for(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn only_a_name_that_says_nothing_takes_the_type() {
        let dir = tempfile::tempdir().unwrap();
        let (zip, elf, text) = (dir.path().join("a.part"), dir.path().join("b.part"), dir.path().join("c.part"));
        std::fs::write(&zip, b"PK\x03\x04\x14\0\0\0").unwrap();
        std::fs::write(&elf, b"\x7FELF\x02\x01\x01").unwrap();
        std::fs::write(&text, b"plain text").unwrap();
        let named = |file: &Path, name: &str| named_by_content(file, dir.path().join(name)).file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(named(&zip, "download"), "download.zip");
        assert_eq!(named(&zip, "get.php"), "get.php.zip");
        assert_eq!(named(&zip, "GET.ASPX"), "GET.ASPX.zip");
        assert_eq!(named(&zip, "report.docx"), "report.docx", "a real extension stays");
        assert_eq!(named(&zip, "data.bin"), "data.bin");
        assert_eq!(named(&elf, "tool"), "tool", "a Linux program keeps its bare name");
        assert_eq!(named(&text, "notes"), "notes");
        assert_eq!(named(&dir.path().join("gone.part"), "x"), "x");
        assert_eq!(type_of(&elf), Some("elf"));
    }
}
