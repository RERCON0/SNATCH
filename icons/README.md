# SNATCH icons

- **GUI (`snatch-app`)**: selected concept 01, folded ribbon S.
- **CLI (`snatch`)**: selected concept 02, orbital S.
- Both use a black mark on a white rounded square with transparent outer corners.

`source/mark-app.png` and `source/mark-cli.png` are the approved transparent
logo masters, finalized with the built-in image generation tool. The associated
prompts are saved alongside them. The marks are original designs for SNATCH.

Run `python icons/export_icons.py` with Pillow to export:

- `icon-app.png` and `icon-cli.png`: 1024-pixel PNG masters.
- `icon-app.ico` and `icon-cli.ico`: 16, 20, 24, 32, 40, 48, 64, 96, 128 and 256 pixels.
- `icon-256.png`: the GUI window/taskbar icon, matching `icon-app.ico`.

The export preserves each mark's aspect ratio and places it on the same flat
white tile; it does not regenerate the selected design.
