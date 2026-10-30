export interface WallpaperOption {
  id: string;
  name: string;
  type: "gradient" | "image";
  value: string;
}

export const BUILTIN_WALLPAPERS: WallpaperOption[] = [
  {
    id: "nebula",
    name: "星云",
    type: "gradient",
    value:
      "radial-gradient(ellipse at 20% 30%, rgba(120, 80, 200, 0.35) 0%, transparent 50%), radial-gradient(ellipse at 80% 70%, rgba(60, 140, 220, 0.3) 0%, transparent 55%), radial-gradient(ellipse at 50% 50%, rgba(20, 30, 60, 1) 0%, rgba(5, 8, 18, 1) 100%)",
  },
  {
    id: "aurora",
    name: "极光",
    type: "gradient",
    value:
      "linear-gradient(135deg, rgba(10, 40, 60, 1) 0%, rgba(20, 80, 90, 0.8) 30%, rgba(60, 180, 160, 0.6) 50%, rgba(100, 100, 200, 0.5) 70%, rgba(15, 25, 50, 1) 100%)",
  },
  {
    id: "sunset",
    name: "暮色",
    type: "gradient",
    value:
      "linear-gradient(180deg, rgba(20, 10, 40, 1) 0%, rgba(80, 30, 60, 0.7) 35%, rgba(180, 80, 60, 0.5) 60%, rgba(40, 20, 50, 1) 100%)",
  },
  {
    id: "forest",
    name: "深林",
    type: "gradient",
    value:
      "radial-gradient(ellipse at 30% 80%, rgba(20, 60, 40, 0.8) 0%, transparent 60%), radial-gradient(ellipse at 70% 20%, rgba(40, 80, 60, 0.6) 0%, transparent 50%), linear-gradient(180deg, rgba(10, 25, 20, 1) 0%, rgba(5, 15, 10, 1) 100%)",
  },
  {
    id: "ocean",
    name: "深海",
    type: "gradient",
    value:
      "linear-gradient(180deg, rgba(10, 30, 60, 1) 0%, rgba(20, 60, 100, 0.7) 40%, rgba(30, 90, 140, 0.5) 70%, rgba(5, 20, 40, 1) 100%)",
  },
];

export function getWallpaperValue(
  wallpaperId: string,
  customWallpapers: string[],
): string {
  const builtin = BUILTIN_WALLPAPERS.find((w) => w.id === wallpaperId);
  if (builtin) return builtin.value;
  const customIndex = parseInt(wallpaperId.replace("custom-", ""), 10);
  if (!isNaN(customIndex) && customWallpapers[customIndex]) {
    return customWallpapers[customIndex];
  }
  return BUILTIN_WALLPAPERS[0].value;
}
