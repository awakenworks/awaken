#!/usr/bin/env node
/**
 * One-shot mdbook → Starlight migration.
 *
 * Walks docs/book/src (EN) and docs/book/src/zh-CN (ZH), and for every
 * Markdown file emits an apps/www/src/content/docs/{,zh-cn/}<same-path>
 * with Starlight frontmatter, link rewriting, and code-block cleanup.
 *
 * Side effects:
 *   - Wipes apps/www/src/content/docs/**\/*.md and **\/*.mdx EXCEPT
 *     files named index.mdx (hand-tuned splash pages stay put).
 *   - Writes apps/www/sidebar.generated.json next to astro.config.mjs;
 *     hand-paste the array into starlight({ sidebar: [...] }).
 *
 * Run from repo root:
 *   node scripts/migrate-mdbook.mjs
 */

import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const srcEn = path.join(repoRoot, "docs/book/src");
const srcZh = path.join(repoRoot, "docs/book/src/zh-CN");
const dstRoot = path.join(repoRoot, "apps/www/src/content/docs");
const sidebarOut = path.join(repoRoot, "apps/www/sidebar.generated.json");

/* ---------- File walking ---------- */

async function walkMarkdown(root, { skipZhRoot = false } = {}) {
  const out = [];
  async function recurse(dir) {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const e of entries) {
      const full = path.join(dir, e.name);
      const rel = path.relative(root, full);
      if (e.isDirectory()) {
        if (skipZhRoot && rel === "zh-CN") continue;
        await recurse(full);
      } else if (e.isFile() && e.name.endsWith(".md") && e.name !== "SUMMARY.md") {
        out.push(rel);
      }
    }
  }
  await recurse(root);
  return out.sort();
}

/* ---------- Transformations ---------- */

const RUST_HIDDEN = /^# (?!\[)/; // mdbook hides `# ` but keeps `#[attr]`

/** Strip mdbook hidden lines + normalize fence languages.
 *
 * mdbook accepts ```rust,ignore / ```rust,no_run / ```rust,should_panic etc.
 * to control its built-in doctest runner. Shiki (used by Starlight) has no
 * idea what those modifiers mean and falls back to plain text with a warning;
 * we drop them so the block highlights normally.
 */
function stripHiddenRustLines(body) {
  const lines = body.split("\n");
  const out = [];
  let inRust = false;
  for (const line of lines) {
    const fenceOpen = line.match(/^(```)(\w+)(,[\w,]+)?\s*$/);
    if (fenceOpen) {
      const [, fence, lang, mods] = fenceOpen;
      const normalizedLang = /^(rust|rs)$/i.test(lang) && mods ? lang : lang;
      // For rust code blocks, drop the ,ignore / ,no_run / ,should_panic suffix.
      const isRust = /^(rust|rs)$/i.test(lang);
      const cleaned = isRust ? `${fence}${normalizedLang}` : line;
      inRust = !inRust && isRust;
      out.push(cleaned);
      continue;
    }
    // Closing fence (just ```)
    if (line.trim() === "```") {
      if (inRust) inRust = false;
      out.push(line);
      continue;
    }
    if (inRust && RUST_HIDDEN.test(line)) continue;
    out.push(line);
  }
  return out.join("\n");
}

/** Rewrite `./foo.md`, `../foo.md`, `path/to/foo.md` → `/path/to/foo/` */
function rewriteLinks(body) {
  return body.replace(
    /(\]\()(\.{1,2}\/)?([^)\s#]+?)\.md(#[^)]*)?\)/g,
    (_match, openBracket, leading, target, anchor) => {
      const cleaned = target.replace(/^\.\//, "");
      return `${openBracket}/${cleaned}/${anchor ?? ""})`;
    },
  );
}

/** Extract H1 + first non-heading paragraph; return { title, description, bodyWithoutH1 } */
function extractFrontmatterParts(body) {
  const lines = body.split("\n");
  let title = "";
  let firstParaLines = [];
  let i = 0;

  // Skip leading blanks
  while (i < lines.length && lines[i].trim() === "") i++;

  // Title from H1
  if (i < lines.length && lines[i].startsWith("# ")) {
    title = lines[i].slice(2).trim();
    i++;
  }

  // Collect first paragraph (skip blanks and code fences and sub-headings)
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      if (firstParaLines.length) break;
      i++;
      continue;
    }
    if (line.startsWith("#")) break;
    if (line.startsWith("```")) break;
    if (line.startsWith("|")) break;
    if (line.startsWith("- ") || line.startsWith("* ")) break;
    firstParaLines.push(line.trim());
    i++;
  }

  let description = firstParaLines.join(" ").replace(/\s+/g, " ");
  // Strip markdown decoration for description tag use
  description = description
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1");
  // Truncate to ~200 chars on a sentence boundary
  if (description.length > 200) {
    description = description.slice(0, 200).replace(/\s+\S*$/, "") + "…";
  }

  // Strip the H1 from body (Starlight renders title from frontmatter)
  const bodyLines = [...lines];
  // Find and remove the H1
  for (let j = 0; j < bodyLines.length; j++) {
    if (bodyLines[j].startsWith("# ")) {
      bodyLines.splice(j, 1);
      // Also strip the blank line that usually follows
      if (bodyLines[j] !== undefined && bodyLines[j].trim() === "") {
        bodyLines.splice(j, 1);
      }
      break;
    }
  }

  return {
    title,
    description,
    bodyWithoutH1: bodyLines.join("\n"),
  };
}

/** YAML-quote a string for frontmatter. */
function yamlQuote(s) {
  if (!s) return '""';
  // If it contains special chars, double-quote and escape
  if (/[:#'"\\]/.test(s)) {
    return `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
  }
  return `"${s}"`;
}

async function migrateFile(srcAbs, locale, relPath) {
  const raw = await fs.readFile(srcAbs, "utf8");
  let body = stripHiddenRustLines(raw);
  body = rewriteLinks(body);

  const { title, description, bodyWithoutH1 } = extractFrontmatterParts(body);

  const frontmatter =
    "---\n" +
    `title: ${yamlQuote(title || path.basename(relPath, ".md"))}\n` +
    (description ? `description: ${yamlQuote(description)}\n` : "") +
    "---\n\n";

  const dstSubdir = locale === "en" ? "" : locale;
  const dstAbs = path.join(dstRoot, dstSubdir, relPath);
  await fs.mkdir(path.dirname(dstAbs), { recursive: true });
  await fs.writeFile(dstAbs, frontmatter + bodyWithoutH1.replace(/\n{3,}/g, "\n\n").trimStart());
}

/* ---------- SUMMARY.md parsing → Starlight sidebar ---------- */

/**
 * Parse a SUMMARY.md into [{ label, items: [{ label, slug }] }].
 * SUMMARY format:
 *   # Section Title
 *   - [Item Name](./relative/path.md)
 */
function parseSummary(text) {
  const sections = [];
  let current = null;
  for (const raw of text.split("\n")) {
    const sectionMatch = raw.match(/^#\s+(.+?)\s*$/);
    if (sectionMatch) {
      // Skip the meta "# Summary" header — it's a label, not a section.
      const label = sectionMatch[1].trim();
      if (/^summary$/i.test(label)) {
        current = null;
        continue;
      }
      current = { label, items: [] };
      sections.push(current);
      continue;
    }
    if (!current) continue;
    const itemMatch = raw.match(/^\s*-\s+\[([^\]]+)\]\((?:\.\/)?(.+?)\.md\)/);
    if (itemMatch) {
      const [, label, target] = itemMatch;
      current.items.push({ label, slug: target.replace(/^\//, "") });
    }
  }
  return sections;
}

/** Merge EN + ZH sidebar trees by slug. Missing ZH entries fall back to EN. */
function mergeSidebar(en, zh) {
  // Index ZH section labels and item labels by slug
  const zhSectionByEnLabel = new Map(); // EN-section-label → ZH-section-label
  // Pair sections by index — both files share the same ordering convention.
  for (let i = 0; i < en.length && i < zh.length; i++) {
    zhSectionByEnLabel.set(en[i].label, zh[i].label);
  }
  const zhItemBySlug = new Map();
  for (const section of zh) {
    for (const item of section.items) zhItemBySlug.set(item.slug, item.label);
  }

  return en.map((section) => ({
    label: section.label,
    translations: zhSectionByEnLabel.has(section.label)
      ? { "zh-CN": zhSectionByEnLabel.get(section.label) }
      : undefined,
    items: section.items.map((item) => {
      const zhLabel = zhItemBySlug.get(item.slug);
      return {
        label: item.label,
        slug: item.slug,
        ...(zhLabel && zhLabel !== item.label
          ? { translations: { "zh-CN": zhLabel } }
          : {}),
      };
    }),
  }));
}

/* ---------- Wipe + run ---------- */

async function wipeGeneratedDocs() {
  async function recurse(dir) {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const e of entries) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) {
        await recurse(full);
        const remaining = await fs.readdir(full);
        if (remaining.length === 0) await fs.rmdir(full);
      } else if (e.isFile()) {
        if (e.name === "index.mdx") continue; // preserve splash
        if (e.name.endsWith(".md") || e.name.endsWith(".mdx")) {
          await fs.unlink(full);
        }
      }
    }
  }
  await recurse(dstRoot);
}

async function main() {
  console.log("→ wiping generated docs (keeps index.mdx)");
  await wipeGeneratedDocs();

  console.log("→ migrating EN");
  const enFiles = await walkMarkdown(srcEn, { skipZhRoot: true });
  for (const rel of enFiles) {
    await migrateFile(path.join(srcEn, rel), "en", rel);
  }
  console.log(`  ${enFiles.length} EN files`);

  console.log("→ migrating ZH");
  const zhFiles = await walkMarkdown(srcZh);
  for (const rel of zhFiles) {
    await migrateFile(path.join(srcZh, rel), "zh-cn", rel);
  }
  console.log(`  ${zhFiles.length} ZH files`);

  console.log("→ generating sidebar");
  const enSummary = await fs.readFile(path.join(srcEn, "SUMMARY.md"), "utf8");
  const zhSummary = await fs.readFile(path.join(srcZh, "SUMMARY.md"), "utf8");
  const sidebar = mergeSidebar(parseSummary(enSummary), parseSummary(zhSummary));
  await fs.writeFile(sidebarOut, JSON.stringify(sidebar, null, 2) + "\n");
  console.log(`  ${sidebarOut}`);

  console.log("✔︎ done");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
