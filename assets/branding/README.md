# HeadroomRoute logo assets

- `light/`: black logo PNGs for light surfaces.
- `dark/`: white logo PNGs for dark surfaces.
- `headroomroute-light.ico`: Windows icon for light surfaces.
- `headroomroute-dark.ico`: Windows icon for dark surfaces.

PNG exports are available at 16, 24, 32, 48, 64, 128, 256, 512, and 1024 pixels.
Each file uses a square transparent canvas with the mark centered at a consistent visual size.

Regenerate the assets from the original source images with:

```powershell
python .\tools\export_logo_assets.py `
    --light <path-to-logo_light.png> `
    --dark <path-to-logo_dark.png> `
    --output .\assets\branding
```
