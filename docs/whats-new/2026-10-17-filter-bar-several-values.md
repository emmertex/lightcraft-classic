### Several keywords, cameras or lenses in the filter bar (issue #213)
- The Camera, Lens and Keyword pickers in the filter bar now take more than one value: the list stays open while you
  click, a photo matching any of the chosen values is shown, and the picker reads `aurora +1`. Different pickers still
  combine, so two keywords and one camera show the photos with either keyword taken with that camera. "Any keyword"
  clears the choice, and the chip above the grid (`Keyword: aurora or blue hour`) removes the whole facet. Agents and
  scripts pass the lists to `library.filter` as `keywords`, `cameras`, `lenses` and `people`.
