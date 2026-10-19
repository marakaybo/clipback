// Список кусков монтажа: node ui/tests/montage.test.js
const assert = require("assert");
const path = require("path");
const { Cuts, History } = require(path.join(__dirname, "..", "montage.js"));

const F = 1 / 60;
const P = (...a) => a.map(([s, e]) => ({ s, e }));
let n = 0;
const t = (name, fn) => {
  fn();
  n++;
};

t("разрез делит кусок на два, у края — ничего", () => {
  const p = P([0, 60]);
  assert.deepStrictEqual(Cuts.cut(p, 10, F), P([0, 10], [10, 60]));
  assert.strictEqual(Cuts.cut(p, 0.005, F), p);
  assert.strictEqual(Cuts.cut(p, 70, F), p);
  // второй разрез — во втором куске
  assert.deepStrictEqual(Cuts.cut(Cuts.cut(p, 10, F), 30, F), P([0, 10], [10, 30], [30, 60]));
});

t("Q убирает левее курсора только в своём куске", () => {
  const p = P([0, 10], [10, 30], [30, 60]);
  assert.deepStrictEqual(Cuts.removeLeft(p, 20, F), P([0, 10], [20, 30], [30, 60]));
  // курсор в начале куска — убирать нечего
  assert.strictEqual(Cuts.removeLeft(p, 10, F), p);
  // курсор у конца куска — уходит весь кусок
  assert.deepStrictEqual(Cuts.removeLeft(p, 29.999, F), P([0, 10], [30, 60]));
});

t("E убирает правее курсора; у начала куска — весь кусок", () => {
  const p = P([0, 10], [10, 30], [30, 60]);
  assert.deepStrictEqual(Cuts.removeRight(p, 20, F), P([0, 10], [10, 20], [30, 60]));
  assert.deepStrictEqual(Cuts.removeRight(p, 10, F), P([0, 10], [30, 60]));
  assert.strictEqual(Cuts.removeRight(p, 59.999, F), p);
});

t("в убранном промежутке Q и E ничего не делают", () => {
  const p = P([0, 10], [20, 30]);
  assert.strictEqual(Cuts.removeLeft(p, 15, F), p);
  assert.strictEqual(Cuts.removeRight(p, 15, F), p);
  assert.strictEqual(Cuts.cut(p, 15, F), p);
});

t("Delete убирает кусок, остальные смыкаются", () => {
  const p = P([0, 10], [10, 30], [30, 60]);
  assert.deepStrictEqual(Cuts.remove(p, 1), P([0, 10], [30, 60]));
  assert.strictEqual(Cuts.remove(p, -1), p);
  assert.strictEqual(Cuts.remove(p, 5), p);
  assert.strictEqual(Cuts.total(Cuts.remove(p, 1)), 40);
});

t("отмена и повтор", () => {
  const h = History();
  assert.strictEqual(h.back(), false);
  const a = P([0, 10], [10, 60]);
  const b = Cuts.remove(a, 0);
  h.apply(a);
  h.apply(b);
  assert.strictEqual(h.apply(b), false, "тот же список — не правка");
  assert.ok(h.back());
  assert.strictEqual(h.cur, a);
  assert.ok(h.back());
  assert.strictEqual(h.cur, null, "до первой правки монтажа нет");
  assert.ok(h.forward());
  assert.ok(h.forward());
  assert.strictEqual(h.cur, b);
  assert.strictEqual(h.forward(), false);
  // новая правка после отмены стирает «повтор»
  h.back();
  h.apply(P([5, 60]));
  assert.strictEqual(h.forward(), false);
  assert.strictEqual(h.undo.length, 2);
});

t("время монтажа ↔ время исходника", () => {
  const p = P([2, 10], [20, 30], [40, 45]); // 8 + 10 + 5
  assert.strictEqual(Cuts.total(p), 23);
  assert.strictEqual(Cuts.srcToOut(p, 0), 0);
  assert.strictEqual(Cuts.srcToOut(p, 5), 3);
  assert.strictEqual(Cuts.srcToOut(p, 15), 8, "из убранного — к началу следующего");
  assert.strictEqual(Cuts.srcToOut(p, 25), 13);
  assert.strictEqual(Cuts.srcToOut(p, 99), 23);
  assert.strictEqual(Cuts.outToSrc(p, 0), 2);
  assert.strictEqual(Cuts.outToSrc(p, 8), 20, "стык — начало второго куска");
  assert.strictEqual(Cuts.outToSrc(p, 20), 42);
  assert.strictEqual(Cuts.outToSrc(p, 99), 45);
  for (const o of [0, 1.5, 7.9, 8, 12.25, 18, 22.9]) assert.ok(Math.abs(Cuts.srcToOut(p, Cuts.outToSrc(p, o)) - o) < 1e-9, `${o}`);
  assert.strictEqual(Cuts.outToSrc([], 3), 0);
});

t("предпросмотр пропускает убранное", () => {
  const p = P([2, 10], [20, 30]);
  assert.strictEqual(Cuts.nextPlayable(p, 0), 2);
  assert.strictEqual(Cuts.nextPlayable(p, 5), 5);
  assert.strictEqual(Cuts.nextPlayable(p, 10), 20);
  assert.strictEqual(Cuts.nextPlayable(p, 12), 20);
  assert.strictEqual(Cuts.nextPlayable(p, 30), null);
  assert.strictEqual(Cuts.nextPlayable([], 0), null);
  assert.deepStrictEqual(Cuts.toPairs(p), [[2, 10], [20, 30]]);
});

console.log(`все ${n} ок`);
