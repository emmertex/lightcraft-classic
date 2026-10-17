## Develop

- **Lens correction cropping:** optical distortion and lateral chromatic aberration corrections now automatically inscribe the crop within the valid post-distortion boundary so corrected photos never sample blank border margins or spike the histogram; **Constrain Crop** is also available in the **Optics** panel, Crop tool handles clamp to the inscribed boundary, and Canon CR2/CR3 MakerNote lens names reach lens profile lookup on import. Fork #211.
