//! The one tar writer, shared by everything this repository ships.
//!
//! Two release artifacts are archives: the conformance corpus and each
//! platform's `libzu` build. A second implementation of a format this
//! dull would be a second set of headers to get subtly wrong, and the
//! wrongness would surface in somebody else's language on the day they
//! unpacked a release.
//!
//! The archives are reproducible. Every field a tar header can carry a
//! timestamp, a user id or a permission bit from the packing machine in
//! is fixed instead, so the same inputs are the same bytes on any
//! machine on any day and a mirror can be compared against a release
//! rather than trusted.

/// The compression level. An artifact is packed once per release and
/// unpacked on every CI run of nine repositories, so the trade is
/// entirely one way.
pub const LEVEL: i32 = 19;

/// A tar of these files, in this order, terminated.
pub fn tar(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (name, bytes) in files {
        append(&mut out, name, bytes)?;
    }
    // Two zero blocks end a tar, and readers that check for them are
    // the reason a truncated archive is detected rather than silently
    // short.
    out.extend_from_slice(&[0u8; 1024]);
    Ok(out)
}

/// The zstd of it, which is what ships.
pub fn compress(tar: &[u8]) -> Result<Vec<u8>, String> {
    zstd::bulk::compress(tar, LEVEL).map_err(|e| format!("compressing the archive: {e}"))
}

/// The permission bits an entry unpacks with.
///
/// Decided from the name rather than read off the packing machine, for
/// the reason every other field here is fixed: a mode taken from the
/// filesystem is a mode that differs between a CI runner and a laptop,
/// and the archive would stop being the same bytes from the same
/// inputs. Names are enough to decide it, because the two things that
/// have to be executable when a user unpacks a release are the
/// program in `bin/` and the shared library beside it, and an archive
/// whose `bin/zu` unpacks unrunnable is the install one-liner failing
/// at its last step.
pub fn mode(name: &str) -> u32 {
    let executable = name.contains("bin/")
        || name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.contains(".so");
    match executable {
        true => 0o755,
        false => 0o644,
    }
}

/// One ustar file header and its content, padded to the block size.
fn append(out: &mut Vec<u8>, name: &str, body: &[u8]) -> Result<(), String> {
    if name.len() > 99 {
        return Err(format!(
            "{name:?} is {} bytes and a ustar name holds 99",
            name.len()
        ));
    }
    let start = out.len();
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    // mode, uid, gid, size, mtime: octal, NUL terminated, fixed.
    write_octal(&mut header[100..108], u64::from(mode(name)));
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], body.len() as u64);
    write_octal(&mut header[136..148], 0);
    // The checksum is computed with its own field read as spaces,
    // which is the one piece of tar that cannot be described without
    // saying it out loud.
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    write_octal(&mut header[148..155], u64::from(sum));
    header[155] = b' ';

    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    let padding = (512 - body.len() % 512) % 512;
    out.resize(out.len() + padding, 0);
    debug_assert_eq!((out.len() - start) % 512, 0);
    Ok(())
}

/// An octal number, right aligned in `field` with a trailing NUL,
/// which is how every numeric field in a tar header is written.
fn write_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&text.as_bytes()[text.len() - digits..]);
    field[digits] = 0;
}

/// The files in a tar, which is what a caller can check an archive
/// against without a tar reader of its own.
pub fn entries(tar: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 512 <= tar.len() {
        let header = &tar[i..i + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&header[..end]).to_string();
        let size = String::from_utf8_lossy(&header[124..135]);
        let size = usize::from_str_radix(size.trim_end_matches('\0').trim(), 8).unwrap_or(0);
        i += 512;
        out.push((name, tar[i..i + size].to_vec()));
        i += size.div_ceil(512) * 512;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_comes_back_out_byte_for_byte() {
        let files = vec![
            ("box/a.txt".to_string(), b"one".to_vec()),
            ("box/b.txt".to_string(), vec![7u8; 1500]),
        ];
        let read = entries(&tar(&files).expect("a tar of two small files"));
        assert_eq!(read, files);
    }

    #[test]
    fn packing_twice_gives_the_same_bytes() {
        let files = vec![("box/a.txt".to_string(), b"one".to_vec())];
        assert_eq!(tar(&files), tar(&files));
    }

    #[test]
    fn a_name_longer_than_a_ustar_header_is_refused() {
        let files = vec![(format!("box/{}", "n".repeat(120)), b"one".to_vec())];
        let err = tar(&files).expect_err("a name that does not fit");
        assert!(err.contains("ustar name holds 99"), "{err}");
    }

    #[test]
    fn an_empty_file_is_a_header_and_no_content() {
        let files = vec![("box/empty".to_string(), Vec::new())];
        let tar = tar(&files).expect("a tar of one empty file");
        assert_eq!(tar.len(), 512 + 1024);
        assert_eq!(entries(&tar), files);
    }

    /// The mode a reader unpacks an entry with, read back out of the
    /// header, because the install one-liner's last step is running
    /// what it unpacked.
    fn mode_of(tar: &[u8], want: &str) -> u32 {
        let mut i = 0;
        while i + 512 <= tar.len() {
            let header = &tar[i..i + 512];
            let end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = String::from_utf8_lossy(&header[..end]).to_string();
            let size = String::from_utf8_lossy(&header[124..135]);
            let size = usize::from_str_radix(size.trim_end_matches('\0').trim(), 8).unwrap_or(0);
            if name == want {
                let mode = String::from_utf8_lossy(&header[100..107]);
                return u32::from_str_radix(mode.trim_end_matches('\0').trim(), 8).expect("octal");
            }
            i += 512 + size.div_ceil(512) * 512;
        }
        panic!("{want} is not in this tar");
    }

    #[test]
    fn what_a_user_has_to_run_unpacks_runnable_and_the_rest_does_not() {
        let files: Vec<(String, Vec<u8>)> = [
            "libzu-x/bin/zu",
            "libzu-x/lib/libzu.so",
            "libzu-x/lib/libzu.dylib",
            "libzu-x/lib/libzu.a",
            "libzu-x/include/zu.h",
            "libzu-x/LICENSE",
        ]
        .iter()
        .map(|name| (name.to_string(), b"x".to_vec()))
        .collect();
        let tar = tar(&files).expect("a tar of a staged package");
        assert_eq!(mode_of(&tar, "libzu-x/bin/zu"), 0o755);
        assert_eq!(mode_of(&tar, "libzu-x/lib/libzu.so"), 0o755);
        assert_eq!(mode_of(&tar, "libzu-x/lib/libzu.dylib"), 0o755);
        assert_eq!(mode_of(&tar, "libzu-x/lib/libzu.a"), 0o644);
        assert_eq!(mode_of(&tar, "libzu-x/include/zu.h"), 0o644);
        assert_eq!(mode_of(&tar, "libzu-x/LICENSE"), 0o644);

        // A versioned soname is still a shared library, and the CLI on
        // Windows is still the CLI.
        assert_eq!(mode("libzu-x/lib/libzu.so.0.5"), 0o755);
        assert_eq!(mode("libzu-x/bin/zu.exe"), 0o755);
        assert_eq!(mode("libzu-x/bin/zu.dll"), 0o755);
    }

    #[test]
    fn compression_is_a_round_trip() {
        let files = vec![("box/a.txt".to_string(), b"one two three".repeat(50))];
        let tar = tar(&files).expect("a tar");
        let archive = compress(&tar).expect("compressed");
        let back = zstd::bulk::decompress(&archive, tar.len()).expect("decompressed");
        assert_eq!(back, tar);
    }
}
