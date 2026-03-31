import { useRef, useState, type FormEvent } from "react";

type ChatInputProps = {
  onSend: (text: string, files?: FileList) => void;
  disabled: boolean;
  themeMode?: "light" | "dark";
};

export function ChatInput({
  onSend,
  disabled,
  themeMode = "light",
}: ChatInputProps) {
  const [input, setInput] = useState("");
  const [previews, setPreviews] = useState<
    { file: File; dataUrl: string }[]
  >([]);
  const fileRef = useRef<HTMLInputElement>(null);
  const isDark = themeMode === "dark";

  const handleFiles = (files: FileList | null) => {
    if (!files) return;
    const images = Array.from(files).filter((f) =>
      f.type.startsWith("image/"),
    );
    for (const file of images) {
      const reader = new FileReader();
      reader.onload = (e) => {
        setPreviews((prev) => [
          ...prev,
          { file, dataUrl: e.target?.result as string },
        ]);
      };
      reader.readAsDataURL(file);
    }
  };

  const removePreview = (index: number) => {
    setPreviews((prev) => prev.filter((_, i) => i !== index));
  };

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    const text = input.trim();
    if ((!text && previews.length === 0) || disabled) return;

    if (previews.length > 0) {
      // Build a DataTransfer to create a FileList
      const dt = new DataTransfer();
      for (const p of previews) dt.items.add(p.file);
      onSend(text || "What is in this image?", dt.files);
    } else {
      onSend(text);
    }
    setInput("");
    setPreviews([]);
    if (fileRef.current) fileRef.current.value = "";
  };

  return (
    <form
      onSubmit={handleSubmit}
      className={
        isDark
          ? "border-t border-slate-700 bg-slate-900/70 px-4 py-3"
          : "border-t border-slate-200 bg-slate-50 px-4 py-3"
      }
    >
      {previews.length > 0 && (
        <div className="mb-2 flex flex-wrap gap-2">
          {previews.map((p, i) => (
            <div key={i} className="group relative">
              <img
                src={p.dataUrl}
                alt={p.file.name}
                className="h-16 w-16 rounded-lg border border-slate-300 object-cover dark:border-slate-600"
              />
              <button
                type="button"
                onClick={() => removePreview(i)}
                className="absolute -right-1 -top-1 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-[10px] text-white opacity-0 transition group-hover:opacity-100"
              >
                x
              </button>
              <div className="mt-0.5 max-w-[64px] truncate text-center text-[9px] text-slate-500">
                {p.file.name}
              </div>
            </div>
          ))}
        </div>
      )}
      <div className="flex gap-2">
        <input
          ref={fileRef}
          type="file"
          accept="image/*"
          multiple
          className="hidden"
          onChange={(e) => handleFiles(e.target.files)}
        />
        <button
          type="button"
          onClick={() => fileRef.current?.click()}
          disabled={disabled}
          title="Attach image"
          className={
            isDark
              ? "rounded-lg border border-slate-600 bg-slate-800 px-2 py-2 text-sm text-slate-300 transition hover:bg-slate-700 disabled:opacity-50"
              : "rounded-lg border border-slate-300 bg-white px-2 py-2 text-sm text-slate-600 transition hover:bg-slate-100 disabled:opacity-50"
          }
        >
          <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 20 20"
            fill="currentColor"
            className="h-4 w-4"
          >
            <path
              fillRule="evenodd"
              d="M1 5.25A2.25 2.25 0 013.25 3h13.5A2.25 2.25 0 0119 5.25v9.5A2.25 2.25 0 0116.75 17H3.25A2.25 2.25 0 011 14.75v-9.5zm1.5 5.81v3.69c0 .414.336.75.75.75h13.5a.75.75 0 00.75-.75v-2.69l-2.22-2.219a.75.75 0 00-1.06 0l-1.97 1.969-4.22-4.219a.75.75 0 00-1.06 0L2.5 11.06zM10 8a1.5 1.5 0 10-3 0 1.5 1.5 0 003 0z"
              clipRule="evenodd"
            />
          </svg>
        </button>
        <input
          data-testid="chat-input"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={
            previews.length > 0
              ? "Add a question about the image..."
              : "Type a message..."
          }
          className={
            isDark
              ? "flex-1 rounded-lg border border-slate-600 bg-slate-950 px-3 py-2 text-sm text-slate-100 outline-none ring-cyan-400 focus:ring-2"
              : "flex-1 rounded-lg border border-slate-300 px-3 py-2 text-sm outline-none ring-cyan-300 focus:ring-2"
          }
        />
        <button
          type="submit"
          disabled={disabled}
          className="rounded-lg bg-cyan-700 px-4 py-2 text-sm font-semibold text-white transition hover:bg-cyan-800 disabled:opacity-50"
        >
          Send
        </button>
      </div>
    </form>
  );
}
