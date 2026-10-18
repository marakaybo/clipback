// Оценка буфера в памяти и предупреждение: node ui/tests/storage.test.js ui/app.js
const fs = require("fs");
const assert = require("assert");

const src = fs.readFileSync(process.argv[2] || "ui/app.js", "utf8").replace(/\r/g, "");
const start = src.indexOf("function mb(bytes)");
const end = src.indexOf("function plural");
const { bufferMem } = new Function(src.slice(start, end) + "\nreturn { bufferMem };")();

const GB = 2 ** 30;
// 5 минут, 8 Мбит/с, 3 дорожки: ≈ 300 МБ видео + 18 МБ звука
let m = bufferMem(8, 5, 3, 16 * GB);
assert.strictEqual(Math.round(m.bytes), 318e6);
assert.strictEqual(m.warn, false);
assert.strictEqual(m.text, "≈ 303 МБ памяти");
// неизвестно, сколько памяти, — не предупреждаем
assert.strictEqual(bufferMem(120, 20, 3, null).warn, false);
// 20 минут 1440p (14 Мбит/с) на ПК с 8 ГБ — больше четверти
m = bufferMem(14, 20, 3, 8 * GB);
assert.strictEqual(m.warn, true);
assert.ok(m.text.startsWith("≈ 2.0 ГБ памяти — много для этого ПК (8 ГБ)"), m.text);
// то же на 32 ГБ — нормально
assert.strictEqual(bufferMem(14, 20, 3, 32 * GB).warn, false);
console.log("storage: ok");
