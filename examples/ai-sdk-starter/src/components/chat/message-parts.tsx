export function TextPart({ text }: { text: string }) {
  return <span> {text}</span>;
}

export function ReasoningPart({
  text,
  state,
}: {
  text?: string;
  state?: string;
}) {
  return (
    <div
      data-testid="reasoning-part"
      className="my-2 rounded border-l-[3px] border-amber-500 bg-amber-50 p-2 text-[0.85em] text-amber-800"
    >
      <strong>Reasoning</strong>
      {state ? ` (${state})` : ""}: {text ?? ""}
    </div>
  );
}

export function SourceUrlPart({
  url,
  title,
}: {
  url: string;
  title?: string;
}) {
  return (
    <div data-testid="source-url-part" className="my-1">
      <strong>Source:</strong>{" "}
      <a
        href={url}
        target="_blank"
        rel="noreferrer"
        className="text-cyan-700 underline"
      >
        {title ?? url}
      </a>
    </div>
  );
}

export function SourceDocumentPart({
  title,
  filename,
  mediaType,
}: {
  title: string;
  filename?: string;
  mediaType?: string;
}) {
  return (
    <div data-testid="source-document-part" className="my-1">
      <strong>Document:</strong> {title}
      {filename ? ` (${filename})` : ""}
      {mediaType ? ` [${mediaType}]` : ""}
    </div>
  );
}

export function FilePart({
  url,
  mediaType,
}: {
  url: string;
  mediaType?: string;
}) {
  const isImage = mediaType?.startsWith("image/") || /\.(png|jpe?g|gif|webp|svg)$/i.test(url);

  if (isImage) {
    return (
      <div data-testid="file-part" className="my-1">
        <img
          src={url}
          alt="uploaded image"
          className="max-h-48 max-w-xs rounded-lg border border-slate-200 dark:border-slate-700"
        />
      </div>
    );
  }

  return (
    <div data-testid="file-part" className="my-1">
      <strong>File:</strong>{" "}
      <a
        href={url}
        target="_blank"
        rel="noreferrer"
        className="text-cyan-700 underline"
      >
        {mediaType ?? "download"}
      </a>
    </div>
  );
}
