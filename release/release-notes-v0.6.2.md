# SNATCH 0.6.2

- Center the startup window after applying the UI scale. Windows positioning uses the actual native window size and monitor work area, including monitors with non-zero origins.
- Use a larger initial window (about 678 x 1062 pixels), square corners, and subdued ASCII/text colors. Remove the bright Windows 11 border.
- Remember the user's normal window size and maximized state across restarts without persisting input text or transient widget state.
- Show resize cursors on all eight edges and corners.
- Update Tokio to fix RUSTSEC-2026-0386 and replace the yanked clipboard-win version.
- Windows x64 portable archive with GUI, CLI, static MSVC CRT and an Ed25519 signed manifest. No separate Visual C++ Redistributable installation is required.
