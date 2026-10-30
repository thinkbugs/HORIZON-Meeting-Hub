import { getWallpaperValue } from "../lib/wallpapers";

interface AmbientBackgroundProps {
  wallpaper?: string;
  customWallpapers?: string[];
}

export function AmbientBackground({
  wallpaper = "nebula",
  customWallpapers = [],
}: AmbientBackgroundProps) {
  const wallpaperValue = getWallpaperValue(wallpaper, customWallpapers);
  const isCustom = wallpaper.startsWith("custom-");

  return (
    <div className="ambient" aria-hidden="true">
      {isCustom ? (
        <div
          className="ambient__wallpaper"
          style={{ backgroundImage: `url(${wallpaperValue})` }}
        />
      ) : (
        <div className="ambient__wallpaper" style={{ background: wallpaperValue }} />
      )}
      <div className="ambient__nebula ambient__nebula--one" />
      <div className="ambient__nebula ambient__nebula--two" />
      <div className="ambient__stars" />
      <div className="ambient__beam" />
      <div className="ambient__rings">
        <span />
        <span />
        <span />
      </div>
      <div className="ambient__earth">
        <div className="ambient__earth-clouds" />
        <div className="ambient__earth-sheen" />
        <div className="ambient__earth-rim" />
      </div>
      <div className="ambient__vignette" />
      <div className="ambient__grain" />
    </div>
  );
}
