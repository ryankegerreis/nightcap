# Nightcap

Nightcap is a sleep timer that gradually dims the screen and lowers the volume, then puts the computer to sleep.

## Language

**Fade**:
The gradual change of brightness or volume from its original level to the target level before the timer ends.
_Avoid_: Ramp, transition

**Original level**:
The brightness or volume captured when a fade begins, which a restore returns to.
_Avoid_: Baseline, saved level

**Restore**:
Returning brightness and volume to their original levels after a cancel or wake.
_Avoid_: Reset, undo

**Captured display**:
A display whose original level was read when a fade began and that takes part in that fade.
_Avoid_: Monitor, screen, panel

**Display group**:
All captured displays of one fade, faded and restored together while each keeps its own proportion.
_Avoid_: Display set, monitor list
