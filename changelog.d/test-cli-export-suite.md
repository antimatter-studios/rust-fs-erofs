### Fixed

- **Recursive export works on Windows.** `fs.erofs export --recursive` looked
  each entry up under a path joined with `\`, so every entry but an empty
  file failed as not found (#307).
