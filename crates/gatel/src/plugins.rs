//! One assembly point shared by execution, validation and capabilities.
pub fn registry() -> Result<gatel_plan::Registry, String> {
    let builder = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle());
    #[cfg(feature = "decode")]
    let builder = builder.bundle(gatel_decode::bundle());
    #[cfg(feature = "cgi")]
    let builder = builder.bundle(gatel_cgi::bundle());
    #[cfg(feature = "scgi")]
    let builder = builder.bundle(gatel_scgi::bundle());
    #[cfg(feature = "fastcgi")]
    let builder = builder.bundle(gatel_fastcgi::bundle());
    #[cfg(feature = "maintenance")]
    let builder = builder.bundle(gatel_maintenance::bundle());
    builder.build()
}
