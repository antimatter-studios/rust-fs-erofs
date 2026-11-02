### Fixed

- **The Windows tools start without the Visual C++ runtime.** rust-fs-core 0.4.3
  links the C runtime statically into the Windows release zips, which did not
  start on a Windows install without it.

### Changed

- **A partial tree export exits 5.** With rust-fs-core 0.4.3, a recursive export
  that published what it could and reported the rest as refused or failed exits
  with the partial status instead of 1.
