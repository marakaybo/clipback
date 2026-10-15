const src = require("fs").readFileSync(process.argv[2], "utf8").replace(/\r\n/g, "\n");
const start = src.indexOf("function parseName");
const end = src.indexOf("const ICONS");
const { clipTitle } = new Function(src.slice(start, end) + "return { clipTitle };")();
const cases = [
  ["Battlefield 6 2026-10-15 21-48-03", "video", "Battlefield 6"],
  ["Clipback 2026-10-15 18-31-54 (отрезок)", "video", "Повтор — отрезок"],
  ["Minecraft 1.21.1 2026-10-15 18-31-54 (отрезок) (2)", "video", "Minecraft 1.21.1 — отрезок"],
  ["Clipback 2026-10-15 18-31-54 (3)", "video", "Повтор"],
  ["Скриншот 2026-10-15 18-31-54", "shot", "Скриншот"],
  ["Мой момент", "video", "Мой момент"],
  ["2026-10-15 18-31-54", "video", "Повтор"],
  ["Battlefield 6 2026-10-15 22-41-24 правка", "shot", "Battlefield 6"],
  ["Запись 2026-10-15 22-41-24", "video", "Запись"],
];
let fail = 0;
for (const [name, kind, want] of cases) {
  const got = clipTitle({ name, kind });
  if (got !== want) { fail++; console.log(`FAIL ${name} -> ${got} (ждали ${want})`); }
}
console.log(fail ? `${fail} ошибок` : `все ${cases.length} ок`);
process.exit(fail ? 1 : 0);
