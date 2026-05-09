# Pre-release Test Checklist — Windows Application Compatibility

Most items below are automated by [app_compat_test.ps1](app_compat_test.ps1).
Run it first, then complete the manual items that require a human.

```powershell
# Prerequisites: cargo build --release, WinFSP installed, WSL + e2fsprogs
.\docs\app_compat_test.ps1
```

---

## Automated (covered by app_compat_test.ps1)

- [x] Copy file from ext4 volume to NTFS — SHA256 verified
- [x] Copy file from NTFS to ext4 volume — SHA256 verified
- [x] Create folder
- [x] Delete file
- [x] Rename file
- [x] Open existing file, edit, save (overwrite)
- [x] Create new file and save to ext4 volume
- [x] Open large file (>100 MB)
- [x] `robocopy /MIR` a directory tree onto ext4 volume
- [x] `xcopy` a directory tree — tree matches source (SHA256 per file)
- [x] Long filenames (200+ chars)
- [x] Unicode filenames (Japanese, Arabic, emoji)
- [x] Files with Windows-reserved names (CON, NUL, PRN) — graceful error
- [x] Simultaneous open from two processes (shared read)
- [x] e2fsck clean after all operations

---

## Manual — requires human interaction

- [ ] Drag-drop file within volume via Explorer
- [ ] Drag-drop file cross-volume via Explorer
- [ ] Open large file (>100 MB) in VS Code — verify no hang/corruption
