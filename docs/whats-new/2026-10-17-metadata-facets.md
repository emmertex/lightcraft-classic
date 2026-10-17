### Metadata counts for filter columns (issue #218, first part)
- `library.facets` lists, for each field asked (camera, lens, keyword, person, label, flag, rating, type, year, ISO,
  focal length), the values the photos in view have and how many photos have each, the way Lightroom Classic's
  Metadata filter columns count them. A field already narrowed in the filter still lists its other values. Scripts and
  agents can use it now; the filter bar's columns and a table view build on it next.
