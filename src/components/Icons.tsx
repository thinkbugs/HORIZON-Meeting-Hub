import type { SVGProps } from "react";

type IconProps = SVGProps<SVGSVGElement> & { size?: number };

function Svg({ size = 20, children, ...props }: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...props}
    >
      {children}
    </svg>
  );
}

export function BrandMark({ size = 30, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 40 40" fill="none" aria-hidden="true" {...props}>
      <circle cx="20" cy="20" r="15.5" stroke="currentColor" strokeOpacity=".22" />
      <path
        d="M5.4 22.4c2.8-9.4 12.1-15.7 21.8-13.4 3.6.8 6.7 2.7 9 5.3"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
      <circle cx="28.8" cy="10" r="2.2" fill="currentColor" />
      <path
        d="M14.2 27.2V15.6M25.8 27.2V15.6M14.2 21.4h11.6"
        stroke="currentColor"
        strokeWidth="2.5"
        strokeLinecap="round"
      />
    </svg>
  );
}

export function WhiteboardGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="6" y="8.5" width="36" height="26" rx="5.5" stroke="currentColor" strokeWidth="2.2" />
      <path
        d="M12.5 27.8c4.8-7.2 9.2-7.2 12.3-1.1 2.4 4.8 5.9 4.1 10.6-2.8"
        stroke="currentColor"
        strokeWidth="2.5"
        strokeLinecap="round"
      />
      <path d="M17 39h14" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <circle cx="34.6" cy="17.2" r="2.5" fill="currentColor" opacity=".45" />
    </svg>
  );
}

export function ProjectionGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="6" y="8" width="30" height="22" rx="4.5" stroke="currentColor" strokeWidth="2.2" />
      <path d="M13 39h16M21 30.5V39" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <path d="M31.5 20.5c4.8 0 8.7 3.9 8.7 8.7" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <path d="M31.5 26.2a3 3 0 0 1 3 3" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
    </svg>
  );
}

export function FilesGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <path
        d="M6.5 15.4a4 4 0 0 1 4-4h8.2l3.8 4.2h15a4 4 0 0 1 4 4v15.5a4 4 0 0 1-4 4h-27a4 4 0 0 1-4-4V15.4Z"
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinejoin="round"
      />
      <path d="M6.5 20.2h35" stroke="currentColor" strokeWidth="2.2" opacity=".55" />
      <path d="M14 27h12" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" opacity=".55" />
    </svg>
  );
}

export function BrowserGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <circle cx="24" cy="24" r="17" stroke="currentColor" strokeWidth="2.2" />
      <path
        d="M9 20h30M9 28h30M24 7c4.5 4.7 6.8 10.4 6.8 17S28.5 36.3 24 41c-4.5-4.7-6.8-10.4-6.8-17S19.5 11.7 24 7Z"
        stroke="currentColor"
        strokeWidth="1.8"
        opacity=".5"
      />
      <path
        d="m29.6 16.9-3.1 8.4-8.4 3.1 3.1-8.4 8.4-3.1Z"
        fill="currentColor"
        fillOpacity=".17"
        stroke="currentColor"
        strokeWidth="1.7"
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function AppsGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      {[13, 24, 35].flatMap((cx) =>
        [13, 24, 35].map((cy) => (
          <rect
            key={`${cx}-${cy}`}
            x={cx - 4.4}
            y={cy - 4.4}
            width="8.8"
            height="8.8"
            rx="2.7"
            fill="currentColor"
            opacity={cx === 24 && cy === 24 ? 0.95 : 0.42}
          />
        )),
      )}
    </svg>
  );
}

export function WifiIcon({ size = 18, ...props }: IconProps) {
  return (
    <Svg size={size} {...props}>
      <path d="M3.5 9.2a13.4 13.4 0 0 1 17 0" />
      <path d="M6.7 12.6a8.7 8.7 0 0 1 10.6 0" />
      <path d="M9.8 16a4 4 0 0 1 4.4 0" />
      <circle cx="12" cy="19" r=".9" fill="currentColor" stroke="none" />
    </Svg>
  );
}

export function BatteryIcon({
  size = 24,
  percent,
  charging,
  ...props
}: IconProps & { percent: number | null; charging?: boolean }) {
  const level = percent === null ? 1 : Math.max(0, Math.min(1, percent / 100));
  return (
    <svg width={size * 1.35} height={size} viewBox="0 0 32 24" fill="none" aria-hidden="true" {...props}>
      <rect x="2" y="6.5" width="23" height="11" rx="3" stroke="currentColor" strokeOpacity=".48" />
      <rect x="4.2" y="8.7" width={18.6 * level} height="6.6" rx="1.6" fill="currentColor" />
      <path d="M27 9.6v4.8" stroke="currentColor" strokeOpacity=".48" strokeWidth="2" strokeLinecap="round" />
      {charging && (
        <path
          d="m15.3 3.9-5 7.5h4l-1.5 5.5 5-7.5h-4l1.5-5.5Z"
          fill="#f6fbff"
          stroke="#06101c"
          strokeWidth="1"
        />
      )}
    </svg>
  );
}

export function GearIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 15.4a3.4 3.4 0 1 0 0-6.8 3.4 3.4 0 0 0 0 6.8Z" />
      <path d="M19.6 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2.06 2.06 0 1 1-2.92 2.92l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1.03 1.56V21a2.06 2.06 0 1 1-4.12 0v-.1a1.7 1.7 0 0 0-1.11-1.56 1.7 1.7 0 0 0-1.87.34l-.06.06a2.06 2.06 0 1 1-2.92-2.92l.06-.06A1.7 1.7 0 0 0 4.4 15a1.7 1.7 0 0 0-1.56-1.03H2.7a2.06 2.06 0 1 1 0-4.12h.1A1.7 1.7 0 0 0 4.4 8.73a1.7 1.7 0 0 0-.34-1.87l-.06-.06A2.06 2.06 0 1 1 6.92 3.9l.06.06a1.7 1.7 0 0 0 1.87.34H9a1.7 1.7 0 0 0 1.03-1.56V2.6a2.06 2.06 0 1 1 4.12 0v.1A1.7 1.7 0 0 0 15.2 4.3a1.7 1.7 0 0 0 1.87-.34l.06-.06a2.06 2.06 0 1 1 2.92 2.92l-.06.06a1.7 1.7 0 0 0-.34 1.87v.08a1.7 1.7 0 0 0 1.56 1.03h.19a2.06 2.06 0 1 1 0 4.12h-.1a1.7 1.7 0 0 0-1.56 1.03Z" />
    </Svg>
  );
}

export function SearchIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="10.8" cy="10.8" r="6.8" />
      <path d="m16 16 4.6 4.6" />
    </Svg>
  );
}

export function PlusIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 5v14M5 12h14" />
    </Svg>
  );
}

export function CloseIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m6 6 12 12M18 6 6 18" />
    </Svg>
  );
}

export function MinimizeIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 12h14" />
    </Svg>
  );
}

export function FullscreenIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8.5 4H4v4.5M15.5 4H20v4.5M20 15.5V20h-4.5M8.5 20H4v-4.5" />
    </Svg>
  );
}

export function PinIcon({ filled = false, ...props }: IconProps & { filled?: boolean }) {
  return (
    <Svg {...props} fill={filled ? "currentColor" : "none"}>
      <path d="M14.5 3.8 20.2 9.5l-2.4.9a3 3 0 0 0-1.5 1l-1.4 1.9.5 3.2-1.4 1.4-7.5-7.5 1.4-1.4 3.2.5 1.9-1.4a3 3 0 0 0 1-1.5l.5-2.4Z" />
      <path d="m9.7 14.3-5.4 5.4" />
    </Svg>
  );
}

export function TrashIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4.5 7h15M9.5 3.8h5M7 7l.8 12.1a1.6 1.6 0 0 0 1.6 1.5h5.2a1.6 1.6 0 0 0 1.6-1.5L17 7M10 11v5.5M14 11v5.5" />
    </Svg>
  );
}

export function EditIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M13.6 5.3 18.7 10.4M4.5 19.5l4.2-.8L19.8 7.6a2.2 2.2 0 0 0-3.1-3.1L5.6 15.6l-1.1 3.9Z" />
    </Svg>
  );
}

export function ChevronLeftIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m14.5 5-7 7 7 7" />
    </Svg>
  );
}

export function CheckIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m5 12.4 4.4 4.4L19 6.8" />
    </Svg>
  );
}

export function GripIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="9" cy="7" r="1" fill="currentColor" stroke="none" />
      <circle cx="15" cy="7" r="1" fill="currentColor" stroke="none" />
      <circle cx="9" cy="12" r="1" fill="currentColor" stroke="none" />
      <circle cx="15" cy="12" r="1" fill="currentColor" stroke="none" />
      <circle cx="9" cy="17" r="1" fill="currentColor" stroke="none" />
      <circle cx="15" cy="17" r="1" fill="currentColor" stroke="none" />
    </Svg>
  );
}

export function ArrowRightIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4.5 12h15M14 6.5l5.5 5.5-5.5 5.5" />
    </Svg>
  );
}

export function UndoIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8.2 5.5 3.8 9.9l4.4 4.4" />
      <path d="M4.4 9.9h9.2a5.8 5.8 0 0 1 0 11.6H9.2" />
    </Svg>
  );
}

export function RedoIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m15.8 5.5 4.4 4.4-4.4 4.4" />
      <path d="M19.6 9.9h-9.2a5.8 5.8 0 0 0 0 11.6h4.4" />
    </Svg>
  );
}

export function PenIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m4.2 19.8 4.9-1 10.3-10.3a2.4 2.4 0 0 0-3.4-3.4L5.7 15.4l-1.5 4.4ZM13.7 7.4l2.9 2.9" />
    </Svg>
  );
}

export function EraserIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m4.5 14.2 8.8-8.8a2.1 2.1 0 0 1 3 0l2.6 2.6a2.1 2.1 0 0 1 0 3l-7 7H7.5l-3-3a.8.8 0 0 1 0-1.1Z" />
      <path d="m10 9 5.7 5.7M9.8 18.8h10" />
    </Svg>
  );
}

export function GridIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <rect x="4" y="4" width="6.5" height="6.5" rx="1.4" />
      <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.4" />
      <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.4" />
      <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.4" />
    </Svg>
  );
}

export function DownloadIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 4v10M7.8 10.2 12 14.4l4.2-4.2M5 19.5h14" />
    </Svg>
  );
}

export function FolderIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3.5 7.5A2.5 2.5 0 0 1 6 5h4l2 2h7.5A2.5 2.5 0 0 1 22 9.5v8A2.5 2.5 0 0 1 19.5 20h-14A2.5 2.5 0 0 1 3 17.5l.5-10Z" />
    </Svg>
  );
}

export function SlidersIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 7h9M17 7h3M4 17h3M11 17h9" />
      <circle cx="15" cy="7" r="2.2" />
      <circle cx="9" cy="17" r="2.2" />
    </Svg>
  );
}

export function ImageIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <rect x="3.5" y="4.5" width="17" height="15" rx="3" />
      <circle cx="9" cy="10" r="1.5" />
      <path d="m4.5 17 4.6-4.3 3.3 2.9 2.8-2.4 4.3 3.8" />
    </Svg>
  );
}

export function InfoIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 10.8v5.3M12 7.7v.1" />
    </Svg>
  );
}

export function AirPlayGlyph(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M6 15.5H4.8A1.8 1.8 0 0 1 3 13.7V6.3a1.8 1.8 0 0 1 1.8-1.8h14.4A1.8 1.8 0 0 1 21 6.3v7.4a1.8 1.8 0 0 1-1.8 1.8H18" />
      <path d="m12 13.6 4.3 6.4H7.7L12 13.6Z" />
    </Svg>
  );
}

export function CastGlyph(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3.5 8.2V6.4A1.9 1.9 0 0 1 5.4 4.5h13.2a1.9 1.9 0 0 1 1.9 1.9v9.5a1.9 1.9 0 0 1-1.9 1.9h-5.1" />
      <path d="M3.5 12.3a5.5 5.5 0 0 1 5.5 5.5M3.5 16.2a1.7 1.7 0 0 1 1.7 1.7M4 20h.1" />
    </Svg>
  );
}

export function RadarIcon(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="12" cy="12" r="8.4" />
      <circle cx="12" cy="12" r="4.2" />
      <path d="M12 3.6v2.2M12 18.2v2.2M3.6 12h2.2M18.2 12h2.2" />
      <path d="m12 12 5.6-3.3" />
    </Svg>
  );
}

export function NotepadGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="10" y="6" width="28" height="36" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <path d="M16 16h16M16 22h16M16 28h10" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" opacity=".6" />
      <path d="M16 10v-2M24 10v-2M32 10v-2" stroke="currentColor" strokeWidth="2" strokeLinecap="round" opacity=".4" />
    </svg>
  );
}

export function ScreenshotGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="6" y="10" width="36" height="28" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <path d="M6 16h36" stroke="currentColor" strokeWidth="1.5" opacity=".4" />
      <rect x="14" y="20" width="20" height="12" rx="2" stroke="currentColor" strokeWidth="1.8" strokeDasharray="3 2" opacity=".6" />
      <circle cx="24" cy="26" r="3" stroke="currentColor" strokeWidth="1.8" />
    </svg>
  );
}

export function ScreenRecGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="6" y="8" width="36" height="26" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <circle cx="24" cy="21" r="7" stroke="currentColor" strokeWidth="2" />
      <circle cx="24" cy="21" r="2.5" fill="currentColor" opacity=".5" />
      <path d="M18 38h12" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <path d="M24 34v4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      <circle cx="38" cy="12" r="3" fill="currentColor" opacity=".7" />
    </svg>
  );
}

export function AnnotationGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="6" y="6" width="36" height="36" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <path d="M14 34l6-16 5 8 4-12 5 20" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
      <circle cx="36" cy="12" r="4" stroke="currentColor" strokeWidth="1.8" />
      <path d="M34 12h4M36 10v4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  );
}

export function OfficeGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="8" y="6" width="32" height="36" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <path d="M16 16h16M16 22h16M16 28h12M16 34h8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" opacity=".5" />
      <path d="M8 12h32" stroke="currentColor" strokeWidth="1.8" opacity=".3" />
      <rect x="12" y="8" width="4" height="2" rx="1" fill="currentColor" opacity=".4" />
      <rect x="18" y="8" width="4" height="2" rx="1" fill="currentColor" opacity=".4" />
      <rect x="24" y="8" width="4" height="2" rx="1" fill="currentColor" opacity=".4" />
    </svg>
  );
}

export function FileTransferGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <path d="M14 28l-4 4v6h6l16-16" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M24 10l8 8-16 16-8-8 16-16z" stroke="currentColor" strokeWidth="2.2" strokeLinejoin="round" />
      <path d="M20 14l8 8" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" opacity=".5" />
      <circle cx="36" cy="36" r="6" stroke="currentColor" strokeWidth="2" />
      <path d="M36 33v6M33 36h6" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
  );
}

export function WelcomeGlyph({ size = 34, ...props }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true" {...props}>
      <rect x="4" y="10" width="40" height="28" rx="4" stroke="currentColor" strokeWidth="2.2" />
      <path d="M4 18h40" stroke="currentColor" strokeWidth="1.5" opacity=".3" />
      <path d="M16 28l4 4 8-8" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" />
      <circle cx="12" cy="14" r="1.5" fill="currentColor" opacity=".5" />
      <circle cx="17" cy="14" r="1.5" fill="currentColor" opacity=".5" />
      <circle cx="22" cy="14" r="1.5" fill="currentColor" opacity=".5" />
    </svg>
  );
}

export function QrCodeIcon({ size = 20, ...props }: IconProps) {
  return (
    <Svg size={size} {...props}>
      <rect x="3" y="3" width="7" height="7" rx="1" />
      <rect x="14" y="3" width="7" height="7" rx="1" />
      <rect x="3" y="14" width="7" height="7" rx="1" />
      <rect x="14" y="14" width="3" height="3" />
      <rect x="18" y="14" width="3" height="3" />
      <rect x="14" y="18" width="3" height="3" />
      <rect x="18" y="18" width="3" height="3" />
      <path d="M3 21v-3M21 3h-3M21 21h-3M21 3v18" />
    </Svg>
  );
}
