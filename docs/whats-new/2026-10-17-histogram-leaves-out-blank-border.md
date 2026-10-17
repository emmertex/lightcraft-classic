### The histogram counts only the photo after lens corrections and perspective (issue #211)
- When a lens correction or a perspective transform pulls the photo's edge inside the frame, the blank border around
  it no longer shows up in the histogram, the vectorscope or the clipping indicators as a spike of light grey. The
  border still shows on the canvas, so the composition is unchanged; Constrain Crop still crops it away. A white wall
  in the photo itself still counts.
