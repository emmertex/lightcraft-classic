## Performance

- **Faster HDR zoom and drags:** HDR previews are prepared for the display off the interface thread, zoomed views keep their last few HDR windows (a pan back or a zoom back in is sharp at once, a pan keeps the part already sharp), and no unseen 8-bit tiles are rendered under them; a 1:1 HDR drag's frames take ~2 ms of interface time instead of ~15 ms. Thanks @Kaan88. (no PR)
