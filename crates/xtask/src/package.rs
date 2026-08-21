//! What a platform's build becomes on the way to being a package.
//!
//! dx/09 C-4 and C-5 ask for four things beside the two libraries: an
//! explicit export list, a pkg-config file, a CMake package config, and
//! an archive a user can unpack and build against. They are one module
//! because they are one layout: every one of those files points at the
//! others by relative path, so writing them separately is writing the
//! same directory tree down four times and finding out at a user's
//! first build that one copy disagreed.
//!
//! The layout is a prefix, which is the whole trick. `pkg-config` finds
//! a `.pc` through `PKG_CONFIG_PATH` and resolves everything else from
//! `${pcfiledir}`; `find_package(zu)` finds a config through
//! `CMAKE_PREFIX_PATH` and resolves everything else from
//! `CMAKE_CURRENT_LIST_DIR`. Both of those work in an unpacked tarball
//! and neither works in a flat directory of five files, which is why
//! the archive is `include/`, `lib/` and `bin/` rather than the build
//! directory with the interesting names picked out of it.
//!
//! Three things are per-target and none of them can be a committed
//! template. The library is called `libzu.so`, `libzu.dylib` or
//! `zu.dll`; a DLL is loaded from `bin/` beside the program and linked
//! against through an import library in `lib/`, which the other two do
//! not have; and a static link needs the system libraries the Rust
//! standard library uses named on the command line, which differ per
//! target and change with the toolchain. That last one is asked of
//! rustc by the build script and passed in here, because a `.pc` whose
//! `Libs.private` is a guess is a `pkg-config --static` line that does
//! not link.
//!
//! The export list is generated from the header rather than written
//! beside it, for the reason the header is the specification (dx/09
//! C-7): a second list would be a second place to add a function to and
//! a first place to forget. It is shipped rather than passed to this
//! repository's own link, because rustc already narrows a cdylib to its
//! `no_mangle` symbols on all three platforms and the case dx/02 §9 is
//! actually about is the other one: a binding that links the static
//! archive inside a plugin of its own, whose linker exports everything
//! unless it is handed a list. That consumer gets the list in the
//! syntax their linker reads.

use std::path::{Path, PathBuf};

use crate::platforms::Platform;

/// Where the C ABI is, relative to the root of the tree.
pub const HEADER: &str = "crates/zu-capi/include/zu.h";

/// Where it is implemented, which is the second half of the check that
/// the ABI is one thing.
pub const SOURCE: &str = "crates/zu-capi/src/lib.rs";

/// Where the ABI's revision is written for everything that is not C.
/// `zu version` reports it from here, so it is the other half of the
/// check that the ABI's number is one number.
pub const REVISION: &str = "crates/zu-common/src/lib.rs";

/// The license that ships in the archive (dx/09 §1.3).
pub const LICENSE: &str = "LICENSE";

/// A file of the package, named by its path inside the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub name: String,
    pub bytes: u64,
    /// What it is there for, which is what the staging step prints.
    pub role: &'static str,
}

impl std::fmt::Display for Staged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<34} {:>8} KiB  {}",
            self.name,
            self.bytes.div_ceil(1024),
            self.role
        )
    }
}

/// A function the header declares and this crate does not define, or
/// the other way round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// Exported, and no declaration for it, so nothing outside can call
    /// it and nothing outside knows it is there.
    Undeclared { name: String },
    /// Declared, and nothing behind it. A caller linking the shared
    /// library finds out at their first call.
    Unimplemented { name: String },
    /// The header and the workspace name different revisions of the
    /// same ABI. Whichever is right, something is reading the wrong one.
    Revision { header: String, workspace: String },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Undeclared { name } => write!(
                f,
                "{SOURCE}: {name} is exported and {HEADER} does not declare it, so nothing can \
                 call it"
            ),
            Note::Unimplemented { name } => write!(
                f,
                "{HEADER}: {name} is declared and {SOURCE} does not define it, so a caller finds \
                 out at their first call"
            ),
            Note::Revision { header, workspace } => write!(
                f,
                "{HEADER}: ZU_ABI_VERSION is {header} and {REVISION} says {workspace}, so a \
                 caller compiling against the header and a caller asking `zu version` are told \
                 different things about one ABI"
            ),
        }
    }
}

/// The ABI, read from the two files that are it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abi {
    /// Every function the header declares, sorted, which is every
    /// symbol the shared library exports.
    pub declared: Vec<String>,
    /// Every `extern "C"` function the crate defines, sorted.
    pub defined: Vec<String>,
    /// The revision the header names, and the one the workspace does.
    pub revision: (String, String),
}

impl Abi {
    /// Reads the header and the source under `root`.
    pub fn read(root: &Path) -> Result<Abi, String> {
        let header = read(&root.join(HEADER))?;
        let revision = read(&root.join(REVISION))?;
        Ok(Abi {
            declared: declared(&header),
            defined: defined(&read(&root.join(SOURCE))?)?,
            revision: (
                quoted_after(&header, "#define ZU_ABI_VERSION")
                    .ok_or_else(|| format!("{HEADER}: no ZU_ABI_VERSION"))?,
                quoted_after(&revision, "pub const C_ABI_VERSION: &str =")
                    .ok_or_else(|| format!("{REVISION}: no C_ABI_VERSION"))?,
            ),
        })
    }

    /// What the two disagree about.
    pub fn check(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        for name in &self.defined {
            if !self.declared.contains(name) {
                notes.push(Note::Undeclared { name: name.clone() });
            }
        }
        for name in &self.declared {
            if !self.defined.contains(name) {
                notes.push(Note::Unimplemented { name: name.clone() });
            }
        }
        let (header, workspace) = &self.revision;
        if header != workspace {
            notes.push(Note::Revision {
                header: header.clone(),
                workspace: workspace.clone(),
            });
        }
        notes
    }

    /// The GNU ld and lld syntax: these global, everything else local.
    /// The `local: *` is the half that matters, since ELF exports
    /// every non-hidden symbol by default.
    pub fn version_script(&self) -> String {
        let mut out = String::from("ZU {\n  global:\n");
        for name in &self.declared {
            out.push_str("    ");
            out.push_str(name);
            out.push_str(";\n");
        }
        out.push_str("  local:\n    *;\n};\n");
        out
    }

    /// The Mach-O syntax, which is one C symbol per line, and a C
    /// symbol on Mach-O carries a leading underscore.
    pub fn symbols_list(&self) -> String {
        self.declared
            .iter()
            .map(|name| format!("_{name}\n"))
            .collect()
    }

    /// The PE syntax, for a consumer building a DLL of their own around
    /// the static archive.
    pub fn module_definition(&self) -> String {
        let mut out = String::from("LIBRARY zu\nEXPORTS\n");
        for name in &self.declared {
            out.push_str("  ");
            out.push_str(name);
            out.push('\n');
        }
        out
    }
}

/// Every function the header declares, sorted.
///
/// The header is C we wrote rather than C in general, so this reads it
/// as such: comments come out, what is left is split at the semicolons
/// that end declarations, and a statement holding a brace is a struct,
/// an enum or the `extern "C" {` that opens the file rather than a
/// function. The name is the identifier before the first parenthesis of
/// what survives.
fn declared(header: &str) -> Vec<String> {
    let mut names = Vec::new();
    for statement in declarations(header).split(';') {
        if statement.contains('{') {
            continue;
        }
        let Some(open) = statement.find('(') else {
            continue;
        };
        let name = identifier_before(&statement[..open]);
        if name.starts_with("zu_") {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The string literal that follows `prefix`, in either language: a C
/// `#define` and a Rust `const` both write one, and both are read here
/// by finding the line and taking what is between the first pair of
/// quotes on it. That is enough because both lines are ours and both
/// are one short literal, and a parser for either language to read one
/// version number would be a parser to maintain.
fn quoted_after(text: &str, prefix: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with(prefix))?;
    let rest = line.split_once('"')?.1;
    Some(rest.split_once('"')?.0.to_owned())
}

/// Every `extern "C"` function the crate exports, read from the source
/// rather than from a built library, so the check needs a checkout and
/// not a linker.
fn defined(source: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut marked = false;
    for line in source.lines() {
        let line = line.trim();
        if line == "#[unsafe(no_mangle)]" {
            marked = true;
            continue;
        }
        if !marked {
            continue;
        }
        marked = false;
        let name = line
            .split("fn ")
            .nth(1)
            .map(|rest| identifier_before(rest.split(['(', '<']).next().unwrap_or("")))
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("{SOURCE}: an exported item that is not a function: {line}"))?;
        if !line.contains("extern \"C\"") {
            return Err(format!(
                "{SOURCE}: {name} is exported and is not extern \"C\""
            ));
        }
        names.push(name);
    }
    names.sort();
    let before = names.len();
    names.dedup();
    if before != names.len() {
        return Err(format!("{SOURCE}: a function is exported twice"));
    }
    Ok(names)
}

/// The identifier at the end of `text`, which is the name in both a C
/// declaration and a Rust signature once the parameters are off.
fn identifier_before(text: &str) -> String {
    let name: String = text
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    name.chars().rev().collect()
}

/// The header with nothing left in it but declarations.
///
/// The comments go because that is where every mention of a function
/// that is not a declaration lives. The preprocessor lines go because a
/// declaration is read as the identifier before the first parenthesis
/// after the last semicolon, and a `#define` with parentheses in its
/// value sits between the two: it would answer for the declaration that
/// follows it, and the function that declaration names would be read as
/// exported and undeclared.
fn declarations(text: &str) -> String {
    strip_comments(text)
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The header without its block comments.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        // An unterminated comment is a header that does not compile, so
        // taking the rest of the file is as good an answer as any.
        let end = rest[at..].find("*/").map_or(rest.len(), |e| at + e + 2);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A linker option written with a dash rather than a slash.
///
/// rustc prints the MSVC list with the spelling MSVC's own linker
/// documents, so `/defaultlib:msvcrt` arrives among the `.lib` names.
/// Both linkers that read it take either spelling, and everything that
/// carries it to them reads a leading slash as the start of a path: to
/// CMake an absolute one, and to the shell git bash runs on Windows a
/// path to rewrite before the compiler ever sees it. So the slash goes
/// once, here, rather than in each of the files this writes.
fn dashed(option: &str) -> String {
    match option.strip_prefix('/') {
        Some(rest) => format!("-{rest}"),
        None => option.to_string(),
    }
}

/// What rustc printed, split into link items rather than into words.
///
/// Every item on every platform is one word but one: Apple's linker
/// takes a framework as `-framework CoreFoundation`, two words that
/// name one thing. Split on the space and the pair becomes two items,
/// and the CMake config then hands the second one to a linker that has
/// been told to expect a framework name: `ld: framework
/// '-lCoreFoundation' not found`, which is the link failing over a
/// library nobody asked for. So the flag keeps the word after it.
fn link_items(printed: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut words = printed.split_whitespace();
    while let Some(word) = words.next() {
        match (word, words.clone().next()) {
            ("-framework", Some(name)) => {
                words.next();
                items.push(format!("-framework {name}"));
            }
            _ => items.push(dashed(word)),
        }
    }
    items
}

/// One platform's package, laid out and written.
#[derive(Debug, Clone)]
pub struct Package<'a> {
    pub platform: &'a Platform,
    pub version: String,
    /// The system libraries a static link needs named after the
    /// archive, as rustc prints them, or empty when nobody asked.
    pub syslibs: Vec<String>,
}

impl<'a> Package<'a> {
    pub fn new(platform: &'a Platform, version: &str, syslibs: &str) -> Package<'a> {
        Package {
            platform,
            version: version.to_string(),
            syslibs: link_items(syslibs),
        }
    }

    /// Whether the shared library is a DLL, which is the one difference
    /// that reaches every file here.
    fn windows(&self) -> bool {
        self.platform.lib.ends_with(".dll")
    }

    /// Where the shared library goes. A DLL is found beside the program
    /// that loads it, and everything else is found by the loader in a
    /// library directory.
    pub fn lib_path(&self) -> String {
        match self.windows() {
            true => format!("bin/{}", self.platform.lib),
            false => format!("lib/{}", self.platform.lib),
        }
    }

    /// What a caller links against for the shared library, which on
    /// Windows is the import library rather than the DLL itself.
    pub fn implib(&self) -> Option<String> {
        self.windows().then(|| "lib/zu.dll.lib".to_string())
    }

    /// The pkg-config file (dx/09 C-4).
    ///
    /// `${pcfiledir}` is what makes it relocatable, which a prebuilt
    /// archive has to be: the user unpacks it where they like and a
    /// prefix baked in at build time would name a directory on a CI
    /// runner. `Libs.private` is read by `pkg-config --static` alone,
    /// so the shared case is unaffected by it being long.
    pub fn pkg_config(&self) -> String {
        let link = match self.windows() {
            true => "-lzu.dll".to_string(),
            false => "-lzu".to_string(),
        };
        let mut out = String::new();
        out.push_str("prefix=${pcfiledir}/../..\n");
        out.push_str("exec_prefix=${prefix}\n");
        out.push_str("libdir=${prefix}/lib\n");
        out.push_str("includedir=${prefix}/include\n\n");
        out.push_str("Name: libzu\n");
        out.push_str("Description: Embedded property-graph database, the C ABI\n");
        out.push_str("URL: https://github.com/tamnd/zu\n");
        out.push_str(&format!("Version: {}\n", self.version));
        out.push_str(&format!("Libs: -L${{libdir}} {link}\n"));
        out.push_str(&format!("Libs.private: {}\n", self.syslibs.join(" ")));
        out.push_str("Cflags: -I${includedir}\n");
        out
    }

    /// The CMake package config (dx/09 C-4), which `find_package(zu)`
    /// finds under `lib/cmake/zu`.
    ///
    /// Two imported targets rather than one, because both library forms
    /// ship and a consumer choosing between them is the reason they
    /// both do. The static one carries the system libraries in its
    /// interface, so a target that links it needs to know nothing about
    /// what the Rust standard library uses.
    pub fn cmake_config(&self) -> String {
        let prefix = "${_zu_prefix}";
        let mut out = String::new();
        out.push_str("# The CMake package config for libzu, generated per platform by\n");
        out.push_str("# `cargo xtask package` and shipped in the archive. Everything is\n");
        out.push_str("# found relative to this file, so the archive works wherever it is\n");
        out.push_str("# unpacked:\n");
        out.push_str("#\n");
        out.push_str("#   find_package(zu REQUIRED)\n");
        out.push_str("#   target_link_libraries(app PRIVATE zu::zu)\n");
        out.push_str("#\n");
        out.push_str("# with the unpacked directory on CMAKE_PREFIX_PATH. Link zu::zu_static\n");
        out.push_str("# instead to put libzu inside your own binary.\n\n");
        out.push_str("get_filename_component(_zu_prefix \"${CMAKE_CURRENT_LIST_DIR}/../../..\" ");
        out.push_str("ABSOLUTE)\n\n");
        out.push_str(&format!(
            "set(zu_VERSION \"{}\")\nset(zu_INCLUDE_DIRS \"{prefix}/include\")\n\n",
            self.version
        ));

        out.push_str("if(NOT TARGET zu::zu)\n");
        out.push_str("  add_library(zu::zu SHARED IMPORTED)\n");
        out.push_str("  set_target_properties(zu::zu PROPERTIES\n");
        out.push_str(&format!(
            "    IMPORTED_LOCATION \"{prefix}/{}\"\n",
            self.lib_path()
        ));
        if let Some(implib) = self.implib() {
            out.push_str(&format!("    IMPORTED_IMPLIB \"{prefix}/{implib}\"\n"));
        }
        out.push_str(&format!(
            "    INTERFACE_INCLUDE_DIRECTORIES \"{prefix}/include\")\n"
        ));
        out.push_str("endif()\n\n");

        out.push_str("if(NOT TARGET zu::zu_static)\n");
        out.push_str("  add_library(zu::zu_static STATIC IMPORTED)\n");
        out.push_str("  set_target_properties(zu::zu_static PROPERTIES\n");
        out.push_str(&format!(
            "    IMPORTED_LOCATION \"{prefix}/lib/{}\"\n",
            self.platform.staticlib
        ));
        // As rustc printed them, up to the spelling `dashed` fixes,
        // because a link item CMake does not recognize is passed through
        // as written and what rustc printed is already the flag the
        // linker wants on every platform.
        out.push_str(&format!(
            "    INTERFACE_LINK_LIBRARIES \"{}\"\n",
            self.syslibs.join(";")
        ));
        out.push_str(&format!(
            "    INTERFACE_INCLUDE_DIRECTORIES \"{prefix}/include\")\n"
        ));
        out.push_str("endif()\n");
        out
    }

    /// The version file beside it, which is what makes
    /// `find_package(zu 0.5)` an answer rather than a wish.
    ///
    /// Compatible means the same major and the same minor, which is
    /// stricter than CMake's usual same-major rule for the reason 0.x
    /// exists: before 1.0 a minor bump is where a break is allowed to
    /// go, and the C ABI is additive-only from its freeze at DX5 rather
    /// than from today (dx/02 R9).
    pub fn cmake_version(&self) -> String {
        let mut out = String::new();
        out.push_str("# Compatible is the same major and the same minor, which is stricter\n");
        out.push_str("# than CMake's same-major default: before 1.0 a minor bump is where a\n");
        out.push_str("# break is allowed to go, and this ABI is additive-only from its\n");
        out.push_str("# freeze rather than from today.\n\n");
        out.push_str(&format!("set(PACKAGE_VERSION \"{}\")\n\n", self.version));
        out.push_str(
            "string(REGEX MATCH \"^[0-9]+\\\\.[0-9]+\" _zu_series \"${PACKAGE_VERSION}\")\n",
        );
        out.push_str(
            "string(REGEX MATCH \"^[0-9]+\\\\.[0-9]+\" _zu_want \"${PACKAGE_FIND_VERSION}\")\n\n",
        );
        out.push_str("if(NOT _zu_series STREQUAL _zu_want)\n");
        out.push_str("  set(PACKAGE_VERSION_COMPATIBLE FALSE)\n");
        out.push_str("elseif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)\n");
        out.push_str("  set(PACKAGE_VERSION_COMPATIBLE FALSE)\n");
        out.push_str("else()\n");
        out.push_str("  set(PACKAGE_VERSION_COMPATIBLE TRUE)\n");
        out.push_str("  if(PACKAGE_VERSION VERSION_EQUAL PACKAGE_FIND_VERSION)\n");
        out.push_str("    set(PACKAGE_VERSION_EXACT TRUE)\n");
        out.push_str("  endif()\n");
        out.push_str("endif()\n");
        out
    }

    /// Lays the package out under `out`, from the tree at `root` and
    /// the build under `built`.
    ///
    /// Every file is copied or generated here rather than in the
    /// workflow, so that the layout is one thing with tests on it and
    /// not a heredoc that a second platform's job spells differently.
    pub fn stage(&self, root: &Path, built: &Path, out: &Path) -> Result<Vec<Staged>, String> {
        let abi = Abi::read(root)?;
        let notes = abi.check();
        if let Some(note) = notes.first() {
            return Err(format!(
                "{note}, and {} more like it. The header is the ABI.",
                notes.len() - 1
            ));
        }

        let mut made = Vec::new();
        let mut copy = |from: &str, to: String, role| -> Result<(), String> {
            let source = built.join(from);
            let bytes = read_bytes(&source)?;
            made.push(write(out, &to, &bytes, role)?);
            Ok(())
        };
        copy(&self.platform.lib, self.lib_path(), "the shared library")?;
        copy(
            &self.platform.staticlib,
            format!("lib/{}", self.platform.staticlib),
            "the static archive",
        )?;
        copy(
            &self.platform.exe,
            format!("bin/{}", self.platform.exe),
            "the CLI",
        )?;
        if let Some(implib) = self.implib() {
            copy("zu.dll.lib", implib, "what a caller links against")?;
        }

        made.push(write(
            out,
            "include/zu.h",
            &read_bytes(&root.join(HEADER))?,
            "the ABI",
        )?);
        made.push(write(
            out,
            LICENSE,
            &read_bytes(&root.join(LICENSE))?,
            "the license",
        )?);

        for (name, text, role) in [
            (
                "lib/pkgconfig/libzu.pc",
                self.pkg_config(),
                "pkg-config --cflags --libs zu",
            ),
            (
                "lib/cmake/zu/zu-config.cmake",
                self.cmake_config(),
                "find_package(zu)",
            ),
            (
                "lib/cmake/zu/zu-config-version.cmake",
                self.cmake_version(),
                "find_package(zu 0.0)",
            ),
            (
                "lib/zu.map",
                abi.version_script(),
                "the export list, GNU ld syntax",
            ),
            (
                "lib/zu.exports",
                abi.symbols_list(),
                "the export list, Mach-O syntax",
            ),
            (
                "lib/zu.def",
                abi.module_definition(),
                "the export list, PE syntax",
            ),
        ] {
            made.push(write(out, name, text.as_bytes(), role)?);
        }
        Ok(made)
    }
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

/// One file of the package, under a path that may be several
/// directories deep, which is the point of the layout.
///
/// The mode comes from the same rule the tar writer uses, so that the
/// staged directory and the archive made from it agree: a `bin/zu`
/// nobody can run is the install one-liner failing at its last step,
/// and this is the copy the smoke test runs before an archive exists.
fn write(out: &Path, name: &str, bytes: &[u8], role: &'static str) -> Result<Staged, String> {
    let path: PathBuf = name
        .split('/')
        .fold(out.to_path_buf(), |at, part| at.join(part));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(crate::tarball::mode(name));
        std::fs::set_permissions(&path, mode)
            .map_err(|e| format!("setting the mode of {}: {e}", path.display()))?;
    }
    Ok(Staged {
        name: name.to_string(),
        bytes: bytes.len() as u64,
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::platforms;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn table() -> platforms::Table {
        platforms::Table::load(&root().join(platforms::PATH)).expect("the committed table loads")
    }

    fn package(target: &str) -> (platforms::Table, String) {
        (table(), target.to_string())
    }

    /// A scratch tree, so a test can stage into something.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zu-package-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
        dir
    }

    #[test]
    fn the_committed_header_is_the_committed_abi() {
        let abi = Abi::read(&root()).expect("both files are there");
        let notes = abi.check();
        assert!(notes.is_empty(), "{notes:#?}");
        assert!(
            abi.declared.len() > 40,
            "{} functions is not a C ABI",
            abi.declared.len()
        );
        assert!(abi.declared.contains(&"zu_database_open".to_string()));
        assert!(
            abi.declared
                .contains(&"zu_result_chunk_col_i64".to_string())
        );
        // Sorted and all of one family, which is what the export list
        // the linker reads has to be.
        let mut sorted = abi.declared.clone();
        sorted.sort();
        assert_eq!(abi.declared, sorted);
        assert!(abi.declared.iter().all(|n| n.starts_with("zu_")));
        // dx/02 §8 is the v0.5 restructure, and the error model, the
        // cancellation calls, the transaction boundaries, the appender,
        // the diagnostics and the frames added to it are all additive,
        // so this is 0.12: the number a binding tests for when it wants
        // to know whether zu_result_arrow is there. The two parts are
        // counts rather than a decimal, so 0.10 is the one after 0.9.
        assert_eq!(abi.revision, ("0.13".to_string(), "0.13".to_string()));
    }

    /// A revision the header and the workspace disagree about is worse
    /// than either of them being wrong, because a caller compiling
    /// against the header and a caller asking the binary then take
    /// different branches over the same installation.
    #[test]
    fn a_header_and_a_binary_that_name_different_revisions_are_reported() {
        let notes = Abi {
            declared: Vec::new(),
            defined: Vec::new(),
            revision: ("0.6".to_string(), "0.5".to_string()),
        }
        .check();
        assert_eq!(
            notes,
            [Note::Revision {
                header: "0.6".to_string(),
                workspace: "0.5".to_string(),
            }]
        );
        assert!(format!("{}", notes[0]).contains("one ABI"), "{}", notes[0]);
    }

    /// Both files write the number the same way, as the first quoted
    /// thing on a line that starts with a fixed prefix, so one reader
    /// serves both and neither language needs parsing.
    #[test]
    fn a_revision_is_read_out_of_either_language() {
        assert_eq!(
            quoted_after("#define ZU_ABI_VERSION \"0.5\"\n", "#define ZU_ABI_VERSION"),
            Some("0.5".to_string())
        );
        assert_eq!(
            quoted_after(
                "/// A doc line.\npub const C_ABI_VERSION: &str = \"0.5\";\n",
                "pub const C_ABI_VERSION: &str ="
            ),
            Some("0.5".to_string())
        );
        // A mention in prose is not a definition, and the prose in both
        // files talks about the constant more than the constant does.
        assert_eq!(
            quoted_after("// see #define ZU_ABI_VERSION for the rule\n", "#define ZU"),
            None
        );
    }

    #[test]
    fn a_declaration_with_nothing_behind_it_is_reported_and_so_is_the_reverse() {
        let abi = Abi {
            declared: vec!["zu_version".to_string(), "zu_ghost".to_string()],
            defined: vec!["zu_version".to_string(), "zu_secret".to_string()],
            revision: ("0.5".to_string(), "0.5".to_string()),
        };
        let notes = abi.check();
        assert!(
            notes.contains(&Note::Undeclared {
                name: "zu_secret".to_string()
            }),
            "{notes:?}"
        );
        assert!(
            notes.contains(&Note::Unimplemented {
                name: "zu_ghost".to_string()
            }),
            "{notes:?}"
        );
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn a_comment_that_names_a_function_is_not_a_declaration() {
        let header = concat!(
            "/* zu_query(conn, q, len) is the call this file is about, and\n",
            " * zu_result_free(res) is how it ends. */\n",
            "typedef struct zu_conn zu_conn;\n",
            "typedef enum zu_status { ZU_OK = 0 } zu_status;\n",
            "const char *zu_version(void);\n",
            "zu_status zu_query(zu_conn *conn, const char *q, size_t len);\n",
        );
        assert_eq!(declared(header), ["zu_query", "zu_version"]);
    }

    #[test]
    fn an_exported_function_is_read_from_the_source_and_a_plain_one_is_not() {
        let source = concat!(
            "#[unsafe(no_mangle)]\n",
            "pub unsafe extern \"C\" fn zu_version() -> *const c_char {\n",
            "}\n",
            "pub extern \"C\" fn not_exported() {}\n",
            "#[unsafe(no_mangle)]\n",
            "pub unsafe extern \"C\" fn zu_query(\n",
            ") -> ZuStatus {}\n",
        );
        assert_eq!(defined(source).expect("reads"), ["zu_query", "zu_version"]);
    }

    #[test]
    fn an_exported_function_that_is_not_extern_c_is_refused() {
        let source = "#[unsafe(no_mangle)]\npub fn zu_version() -> u32 { 1 }\n";
        let error = defined(source).expect_err("a no_mangle that is not extern C");
        assert!(error.contains("is not extern \"C\""), "{error}");
    }

    #[test]
    fn a_dll_is_found_beside_the_program_and_linked_through_its_import_library() {
        let (table, target) = package("x86_64-pc-windows-msvc");
        let platform = table.platform(&target).expect("a row");
        let package = Package::new(platform, "0.5.0", "kernel32.lib ws2_32.lib");
        assert_eq!(package.lib_path(), "bin/zu.dll");
        assert_eq!(package.implib().as_deref(), Some("lib/zu.dll.lib"));
        let cmake = package.cmake_config();
        assert!(cmake.contains("IMPORTED_IMPLIB"), "{cmake}");
        assert!(cmake.contains("bin/zu.dll"), "{cmake}");
        assert!(cmake.contains("lib/zu.lib"), "{cmake}");
        assert!(package.pkg_config().contains("-lzu.dll"));
    }

    #[test]
    fn a_linker_option_that_looks_like_a_path_is_not_left_looking_like_one() {
        let (table, target) = package("x86_64-pc-windows-msvc");
        let platform = table.platform(&target).expect("a row");
        // What rustc prints for this target, which is library names and
        // one option that starts where an absolute path starts.
        let windows = Package::new(
            platform,
            "0.5.0",
            "kernel32.lib ntdll.lib /defaultlib:msvcrt",
        );
        assert_eq!(
            windows.syslibs,
            ["kernel32.lib", "ntdll.lib", "-defaultlib:msvcrt"]
        );
        assert!(
            windows
                .cmake_config()
                .contains("INTERFACE_LINK_LIBRARIES \"kernel32.lib;ntdll.lib;-defaultlib:msvcrt\"")
        );
        assert!(
            windows
                .pkg_config()
                .contains("Libs.private: kernel32.lib ntdll.lib -defaultlib:msvcrt\n")
        );
        // The library names themselves are not options and are left
        // alone, on this platform and on the ones that use -l.
        let (elf, elf_target) = package("x86_64-unknown-linux-gnu");
        let elf = elf.platform(&elf_target).expect("a row");
        assert_eq!(
            Package::new(elf, "0.5.0", "-lgcc_s -lc").syslibs,
            ["-lgcc_s", "-lc"]
        );
    }

    /// Apple names a framework in two words, and the two are one thing
    /// to link. Split them and the CMake list has an item called
    /// CoreFoundation in it, which reaches the linker as -lCoreFoundation
    /// and fails the static smoke link of the darwin row.
    #[test]
    fn a_framework_and_its_name_are_one_item() {
        let (table, target) = package("aarch64-apple-darwin");
        let platform = table.platform(&target).expect("a row");
        // What rustc prints for this target, framework and all.
        let apple = Package::new(
            platform,
            "0.5.0",
            "-liconv -framework CoreFoundation -lSystem -lc -lm",
        );
        assert_eq!(
            apple.syslibs,
            [
                "-liconv",
                "-framework CoreFoundation",
                "-lSystem",
                "-lc",
                "-lm"
            ]
        );
        // One item in the CMake list, which CMake passes through as it
        // stands because it starts with a dash.
        assert!(
            apple.cmake_config().contains(
                "INTERFACE_LINK_LIBRARIES \"-liconv;-framework CoreFoundation;-lSystem;-lc;-lm\""
            ),
            "{}",
            apple.cmake_config()
        );
        // The pkg-config line is words either way, and it reads the
        // same as what rustc printed.
        assert!(
            apple
                .pkg_config()
                .contains("Libs.private: -liconv -framework CoreFoundation -lSystem -lc -lm\n")
        );
        // Two of them, and a trailing flag with nothing after it, which
        // is not a list rustc prints but is one this must not lose.
        let two = Package::new(
            platform,
            "0.5.0",
            "-framework Security -framework CFNetwork",
        );
        assert_eq!(two.syslibs, ["-framework Security", "-framework CFNetwork"]);
        assert_eq!(
            Package::new(platform, "0.5.0", "-lSystem -framework").syslibs,
            ["-lSystem", "-framework"]
        );
    }

    #[test]
    fn an_elf_platform_puts_the_shared_library_where_the_loader_looks() {
        let (table, target) = package("x86_64-unknown-linux-gnu");
        let platform = table.platform(&target).expect("a row");
        let package = Package::new(platform, "0.5.0", "-lgcc_s -lc -lm");
        assert_eq!(package.lib_path(), "lib/libzu.so");
        assert_eq!(package.implib(), None);
        assert!(!package.cmake_config().contains("IMPORTED_IMPLIB"));
        assert!(package.pkg_config().contains("Libs: -L${libdir} -lzu\n"));
        assert!(
            package
                .pkg_config()
                .contains("Libs.private: -lgcc_s -lc -lm\n")
        );
        assert!(
            package
                .cmake_config()
                .contains("INTERFACE_LINK_LIBRARIES \"-lgcc_s;-lc;-lm\"")
        );
    }

    /// The one property every file in the layout depends on: the `.pc`
    /// is two directories under the prefix and the CMake config is
    /// three, and each of them walks back up by that many.
    #[test]
    fn every_generated_file_finds_the_prefix_from_where_it_sits() {
        let (table, target) = package("aarch64-apple-darwin");
        let platform = table.platform(&target).expect("a row");
        let package = Package::new(platform, "0.5.0", "-lSystem");

        // lib/pkgconfig/libzu.pc, so two levels up.
        assert!(
            package
                .pkg_config()
                .starts_with("prefix=${pcfiledir}/../..\n")
        );
        // lib/cmake/zu/zu-config.cmake, so three.
        assert!(
            package
                .cmake_config()
                .contains("\"${CMAKE_CURRENT_LIST_DIR}/../../..\""),
            "{}",
            package.cmake_config()
        );
    }

    #[test]
    fn a_version_file_accepts_its_own_series_and_refuses_the_next() {
        let (table, target) = package("aarch64-apple-darwin");
        let platform = table.platform(&target).expect("a row");
        let text = Package::new(platform, "0.5.0", "").cmake_version();
        assert!(text.contains("set(PACKAGE_VERSION \"0.5.0\")"), "{text}");
        assert!(text.contains("PACKAGE_VERSION_COMPATIBLE FALSE"), "{text}");
        assert!(text.contains("PACKAGE_VERSION_EXACT TRUE"), "{text}");
    }

    #[test]
    fn the_export_list_is_written_in_all_three_syntaxes() {
        let abi = Abi::read(&root()).expect("the committed ABI");
        let map = abi.version_script();
        let exports = abi.symbols_list();
        let def = abi.module_definition();
        assert!(map.starts_with("ZU {\n  global:\n"), "{map}");
        assert!(
            map.contains("  local:\n    *;\n"),
            "an ELF list that does not hide the rest exports the rest: {map}"
        );
        assert!(def.starts_with("LIBRARY zu\nEXPORTS\n"), "{def}");
        for name in &abi.declared {
            assert!(
                map.contains(&format!("    {name};\n")),
                "zu.map lacks {name}"
            );
            assert!(
                exports.contains(&format!("_{name}\n")),
                "zu.exports lacks {name}"
            );
            assert!(def.contains(&format!("  {name}\n")), "zu.def lacks {name}");
        }
        assert_eq!(exports.lines().count(), abi.declared.len());
        assert_eq!(def.lines().count(), abi.declared.len() + 2);
    }

    #[test]
    fn a_staged_package_is_a_prefix_a_build_system_can_find_things_in() {
        let dir = scratch("stage");
        let built = dir.join("built");
        let out = dir.join("prefix");
        std::fs::create_dir_all(&built).expect("writes");
        for (name, bytes) in [("libzu.dylib", 4096), ("libzu.a", 8192), ("zu", 2048)] {
            std::fs::write(built.join(name), vec![7u8; bytes]).expect("writes");
        }

        let (table, target) = package("aarch64-apple-darwin");
        let platform = table.platform(&target).expect("a row");
        let staged = Package::new(platform, "0.5.0", "-lSystem")
            .stage(&root(), &built, &out)
            .expect("stages");

        let mut names: Vec<&str> = staged.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "LICENSE",
                "bin/zu",
                "include/zu.h",
                "lib/cmake/zu/zu-config-version.cmake",
                "lib/cmake/zu/zu-config.cmake",
                "lib/libzu.a",
                "lib/libzu.dylib",
                "lib/pkgconfig/libzu.pc",
                "lib/zu.def",
                "lib/zu.exports",
                "lib/zu.map",
            ]
        );
        assert!(staged.iter().all(|s| s.bytes > 0), "{staged:?}");
        // The files are where the generated ones say they are, which is
        // the whole of whether pkg-config and CMake work.
        assert!(out.join("lib/pkgconfig/libzu.pc").is_file());
        assert!(out.join("lib/cmake/zu/zu-config.cmake").is_file());
        assert!(out.join("include/zu.h").is_file());

        // The CLI is staged runnable, since the smoke test runs this
        // copy and a user runs the one in the archive.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |name: &str| {
                std::fs::metadata(out.join(name))
                    .expect("staged")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("bin/zu"), 0o755);
            assert_eq!(mode("lib/libzu.dylib"), 0o755);
            assert_eq!(mode("include/zu.h"), 0o644);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_that_produced_one_library_form_does_not_stage() {
        let dir = scratch("half");
        let built = dir.join("built");
        std::fs::create_dir_all(&built).expect("writes");
        std::fs::write(built.join("libzu.dylib"), vec![7u8; 16]).expect("writes");
        std::fs::write(built.join("zu"), vec![7u8; 16]).expect("writes");

        let (table, target) = package("aarch64-apple-darwin");
        let platform = table.platform(&target).expect("a row");
        let error = Package::new(platform, "0.5.0", "")
            .stage(&root(), &built, &dir.join("prefix"))
            .expect_err("no static archive");
        assert!(error.contains("libzu.a"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
