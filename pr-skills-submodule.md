Fixes the red `windows-preview` job (it also fails on `main`, e.g. [run 44893738594](https://github.com/miyako/skills/actions/runs/44893738594)).

## Cause

Both Windows runners fail before the project compiles:

```
MSVC\14.52.37025\include\yvals_core.h:923:1: error: static assertion failed:
error STL1000: Unexpected compiler version, expected Clang 22 or newer.
```

`windows-latest` / `windows-11-arm` moved to Visual Studio 18 / MSVC 14.52, whose STL asserts the compiler version, while the pinned Swift toolchain carries Clang 21. I tried `swift-6.4.3-release` and it fails identically, so no released toolchain can satisfy that check yet.

## Fix

`miyako/4d-form#3` passes the documented opt-out in the Windows build script and in its Windows test steps, which is what swiftlang/swift#79709 and #79741 propose upstream. Both Windows legs of the 4d-form CI are green with it.

This PR bumps the `vendor/4d-form` pointer to that commit. Nothing else changes.

**Merge after `4d-form#3`** so the pointer names a commit on `main`.
