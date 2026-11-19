//! Builds libghostty-vt from the Ghostty release in build.zig.zon and
//! installs it where the libghostty-vt-sys build script looks:
//! `<prefix>/lib/libghostty-vt.a` and `<prefix>/include/ghostty/vt.h`.
//!
//! Zig's package manager only fetches the release here. The library is then
//! built by Ghostty's own top-level build (`zig build -Demit-lib-vt` in the
//! fetched package): only that build merges the SIMD archives into one
//! self-contained `libghostty-vt.a`; as a dependency Ghostty hands out the
//! bare archive and expects Zig's `linkLibrary` to add the rest.

const std = @import("std");

pub fn build(b: *std.Build) void {
    // What the libghostty-vt-sys build script passes, forwarded as given.
    const optimize = b.option([]const u8, "optimize", "Zig optimize mode") orelse "ReleaseFast";
    const cpu = b.option([]const u8, "cpu", "Target CPU model") orelse "baseline";
    const target = b.option([]const u8, "target", "Target triple (cross builds)");
    // Always libghostty-vt only, without the xcframework or the app.
    _ = b.option(bool, "emit-lib-vt", "Ignored: always libghostty-vt");
    _ = b.option(bool, "emit-xcframework", "Ignored: no xcframework");
    _ = b.option([]const u8, "app-runtime", "Ignored: no app");

    const ghostty = b.dependency("ghostty", .{});
    const run = b.addSystemCommand(&.{
        b.graph.zig_exe,
        "build",
        "-Demit-lib-vt=true",
        "-Demit-xcframework=false",
        "-Dapp-runtime=none",
        b.fmt("-Doptimize={s}", .{optimize}),
        b.fmt("-Dcpu={s}", .{cpu}),
    });
    if (target) |triple| run.addArg(b.fmt("-Dtarget={s}", .{triple}));
    run.addArg("--prefix");
    const prefix = run.addOutputDirectoryArg("ghostty-install");
    run.setCwd(ghostty.path(""));
    // A new release in build.zig.zon must rebuild (the cwd is no input).
    run.addFileInput(b.path("build.zig.zon"));
    // The fetched package sits inside SuperGIT's checkout (zig-pkg/), and
    // Ghostty's build asks git in its build root for its version: git finds
    // SuperGIT's repository, and on a tagged commit (v0.3.0) Ghostty panics
    // ("tagged releases must be in vX.Y.Z format matching build.zig") since
    // the tag is not its own. Point git at no repository: Ghostty then takes
    // its "git not available" path and builds as its own dev version.
    run.setEnvironmentVariable("GIT_DIR", "/nonexistent/supergit-not-a-ghostty-repo");

    b.installDirectory(.{
        .source_dir = prefix.path(b, "lib"),
        .install_dir = .lib,
        .install_subdir = "",
        .include_extensions = &.{"libghostty-vt.a"},
    });
    b.installDirectory(.{
        .source_dir = prefix.path(b, "include"),
        .install_dir = .header,
        .install_subdir = "",
    });
}
