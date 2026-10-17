//! Directories private to the user, for app state another local account must
//! not read or plant files in (the browser's instance dirs, widget script
//! copies).

/// A directory builder that creates `0700` directories (unix).
pub fn private_dirs(recursive: bool) -> std::fs::DirBuilder {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}
