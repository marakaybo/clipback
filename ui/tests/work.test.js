// Слой прогресса: node ui/tests/work.test.js
const assert = require("assert");
const path = require("path");
const { Work } = require(path.join(__dirname, "..", "work.js"));

// проценты: 100 — только когда действительно готово
assert.strictEqual(Work.percent(0), 0);
assert.strictEqual(Work.percent(0.424), 42);
assert.strictEqual(Work.percent(1), 99);
assert.strictEqual(Work.percent(1, true), 100);
assert.strictEqual(Work.percent(-1), 0);
assert.strictEqual(Work.percent(NaN), 0);
assert.strictEqual(Work.text("Сохраняю отрезок…", 0.42), "Сохраняю отрезок… 42 %");

// ход идёт только вперёд; мелкий откат — шум, большой — новая попытка сжатия
assert.deepStrictEqual(Work.next(0.5, 0.6), { f: 0.6, retry: false });
assert.deepStrictEqual(Work.next(0.5, 0.45), { f: 0.5, retry: false });
assert.deepStrictEqual(Work.next(0.9, 0.05), { f: 0.05, retry: true });
assert.deepStrictEqual(Work.next(0.3, undefined), { f: 0.3, retry: false });
console.log("work: ok");
