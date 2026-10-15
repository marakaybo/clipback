// Проверка разбора «что нового» (Markdown со страницы релиза): node ui/tests/notes.test.js ui/app.js
const fs = require("fs");
const assert = require("assert");

const src = fs.readFileSync(process.argv[2] || "ui/app.js", "utf8").replace(/\r/g, "");
const start = src.indexOf("  function notesHtml(md) {");
const end = src.indexOf("\n  }\n", start);
const notesHtml = new Function(src.slice(start, end + 4) + "\nreturn notesHtml;")();

const cases = [
  ["**Новое**\n- пункт", "<h4>Новое</h4><ul><li>пункт</li></ul>"],
  ["- **A** b\n  - c `d`", "<ul><li><b>A</b> b</li><ul><li>c <code>d</code></li></ul></ul>"],
  ["## Заголовок\nТекст", "<h4>Заголовок</h4><p>Текст</p>"],
  ["- <img src=x onerror=alert(1)>", "<ul><li>&lt;img src=x onerror=alert(1)&gt;</li></ul>"],
  ["", ""],
];
for (const [md, html] of cases) assert.strictEqual(notesHtml(md), html, md);
console.log(`все ${cases.length} ок`);
