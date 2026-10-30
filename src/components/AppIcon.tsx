import type { AppEntry } from "../types";
import {
  AnnotationGlyph,
  AppsGlyph,
  BrowserGlyph,
  FileTransferGlyph,
  FilesGlyph,
  NotepadGlyph,
  OfficeGlyph,
  ProjectionGlyph,
  ScreenRecGlyph,
  ScreenshotGlyph,
  WelcomeGlyph,
  WhiteboardGlyph,
} from "./Icons";

const glyphs: Record<string, React.ComponentType<{ size?: number }>> = {
  whiteboard: WhiteboardGlyph,
  projection: ProjectionGlyph,
  files: FilesGlyph,
  browser: BrowserGlyph,
  notepad: NotepadGlyph,
  screenshot: ScreenshotGlyph,
  screenrec: ScreenRecGlyph,
  annotation: AnnotationGlyph,
  office: OfficeGlyph,
  filetransfer: FileTransferGlyph,
  welcome: WelcomeGlyph,
};

export function monogram(name: string): string {
  const clean = name.trim();
  if (!clean) return "A";
  if (/[\u3400-\u9fff]/.test(clean)) return clean.replace(/\s/g, "").slice(0, 1);
  const words = clean.split(/[\s_-]+/).filter(Boolean);
  if (words.length > 1) return words.slice(0, 2).map((word) => word[0]).join("").toUpperCase();
  return clean.slice(0, 2).toUpperCase();
}

export function EntryIcon({
  entry,
  className = "",
  glyphSize = 36,
}: {
  entry: AppEntry;
  className?: string;
  glyphSize?: number;
}) {
  const Glyph = entry.builtin ? glyphs[entry.builtin] : null;

  return (
    <span
      className={`entry-icon accent-${entry.accent} ${Glyph ? "entry-icon--system" : "entry-icon--app"} ${className}`}
      aria-hidden="true"
    >
      {entry.builtin ? (
        Glyph && <Glyph size={glyphSize} />
      ) : entry.icon ? (
        <img src={entry.icon} alt="" draggable={false} />
      ) : (
        <span className="entry-icon__monogram">{monogram(entry.name)}</span>
      )}
    </span>
  );
}

export function AllAppsIcon({ size = 34 }: { size?: number }) {
  return (
    <span className="entry-icon entry-icon--apps" aria-hidden="true">
      <AppsGlyph size={size} />
    </span>
  );
}
