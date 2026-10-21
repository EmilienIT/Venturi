# Camera Shake

Drag **Camera Shake** from the Effects panel onto a video, title, solid
color or compound clip. On an adjustment clip it moves the composed tracks
below it. Its controls appear under **Properties > Video > Filters**.

| Control | Meaning |
|---|---|
| Motion scale | Overall strength, from 0 (unchanged) to 2. Animate it for an impact or to fade the shake in and out. |
| Speed (Hz) | Rate of the procedural movement, from 0 to 20. Zero holds the motion phase. |
| Pan / Tilt (%) | Horizontal and vertical amplitude as percentages of the timeline width and height. |
| Rotation (°) | Maximum angular amplitude before motion scale. |
| Zoom (%) | Amplitude of the zoom variation before motion scale. |
| Smoothness | At 1 the movement is slow and smooth; lower values add faster variations. |
| Compensate edges | Adds an overscan margin for the shake on full-frame clips. Disable it for titles or intentional transparent borders. |
| Seed / New variation | A repeatable variation of the movement. |

All seven numeric motion controls support keyframes, curve editing and
per-parameter reset. Seed and edge compensation are static. The filter
header resets all controls; its checkbox bypasses the effect. Copy/paste
attributes and library presets carry the settings and keyframes.

The motion is deterministic, evaluated in source time and composed with
the clip's manual transform before pixel filters. Seeking, splitting or
moving a clip does not restart its motion. Retiming also retimes the shake.
Preview, nested timelines and export use the same evaluation. Speed
animation integrates the speed curve so changing speed does not jump to
an unrelated part of the motion.

Edge compensation covers the shake envelope on an otherwise full-frame
image; existing crop, letterboxing, placement or transparency can still
leave uncovered areas. It does not synthesize pixels or apply motion blur.

The native project format and Venturi's OTIO metadata preserve the effect.
Other editors do not receive a native Camera Shake effect or baked
keyframes through OTIO.
