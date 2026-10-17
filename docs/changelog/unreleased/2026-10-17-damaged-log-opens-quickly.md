## Fixes

- **Damaged catalog log opens quickly:** a long damaged line in `catalog.log` no longer keeps the library from
  opening for minutes, and a retried record is still found when its smart-album rules hold nested values. Thanks
  @skybluu and @endthestart. Fork #253. (⇠ #2153)
