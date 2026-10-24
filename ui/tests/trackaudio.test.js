// Звук плеера по дорожкам: node ui/tests/trackaudio.test.js
const assert = require("assert");
const path = require("path");
const { TrackAudio } = require(path.join(__dirname, "..", "trackaudio.js"));

const NAMES = ["Всё вместе", "Звук системы", "Микрофон", "Discord"];
const all = () => true;
const full = () => 100;
const gain = (p, i) => p.tracks.find((t) => t.index === i).gain;
const near = (a, b) => assert.ok(Math.abs(a - b) < 1e-9, `${a} ≈ ${b}`);

// ничего не меняли — играет само видео (первая дорожка уже «Всё вместе»)
assert.strictEqual(TrackAudio.plan(NAMES, all, full, null).native, true);
// громкость приложения при включённой системе не слышна и в сохранённом звуке
assert.strictEqual(TrackAudio.plan(NAMES, all, (i) => (i === 3 ? 50 : 100), null).native, true);

// выключили систему — слышно микрофон и Discord, а не всё сразу
let p = TrackAudio.plan(NAMES, (i) => i !== 1, full, null);
assert.strictEqual(p.native, false);
assert.deepStrictEqual(p.tracks.map((t) => t.index), [1, 2, 3], "«Всё вместе» отдельно не играет");
assert.strictEqual(gain(p, 1), 0);
assert.strictEqual(gain(p, 2), 1);
assert.strictEqual(gain(p, 3), 1);

// выключили микрофон — только система; Discord уже в ней, второй раз не играет
p = TrackAudio.plan(NAMES, (i) => i !== 2, full, null);
assert.deepStrictEqual([gain(p, 1), gain(p, 2), gain(p, 3)], [1, 0, 0]);

// громкость дорожки и выравнивание голоса — как в сохранённом «Всё вместе»
p = TrackAudio.plan(NAMES, all, (i) => (i === 2 ? 150 : 100), { sys: 2, mic: 0.5 });
assert.strictEqual(p.native, false);
near(gain(p, 1), 2);
near(gain(p, 2), 0.75);
// одна из двух выключена — выравнивания нет, дорожка как есть
p = TrackAudio.plan(NAMES, (i) => i !== 1, full, { sys: 2, mic: 0.5 });
assert.strictEqual(gain(p, 2), 1);

// клип с одной дорожкой: видео, пока громкость 100 %
assert.strictEqual(TrackAudio.plan(["Звук"], all, full, null).native, true);
p = TrackAudio.plan(["Звук"], all, () => 40, null);
assert.strictEqual(p.native, false);
near(gain(p, 0), 0.4);
// без «Всё вместе»: система и приложение, приложение выключено — видео как есть
assert.strictEqual(TrackAudio.plan(["Звук системы", "Discord"], (i) => i === 0, full, null).native, true);

// выравнивание по пикам: тихий голос поднимается, громкий — опускается, игра подтягивается
const flat = (v, n = 50) => Array(n).fill(v);
let lv = TrackAudio.level(flat(0.3), flat(0.05));
assert.strictEqual(lv.sys, 1);
assert.ok(lv.mic > 5 && lv.mic <= 6, `тихий голос ×${lv.mic}`);
lv = TrackAudio.level(flat(0.02), flat(0.5));
assert.strictEqual(lv.mic, 0.5);
assert.ok(lv.sys > 1 && lv.sys <= 2.8, `тихая игра ×${lv.sys}`);
// паузы (тишина) в оценку не входят
assert.deepStrictEqual(TrackAudio.level([...flat(0.2), ...flat(0, 500)], [...flat(0.2), ...flat(0, 500)]), TrackAudio.level(flat(0.2), flat(0.2)));
// нет голоса или игры — ничего не трогаем
assert.deepStrictEqual(TrackAudio.level(flat(0.3), flat(0)), { sys: 1, mic: 1 });
assert.deepStrictEqual(TrackAudio.level(null, null), { sys: 1, mic: 1 });

// синхронизация: мелкое расхождение терпим, заметное — переставляем, с учётом задержки вывода
assert.strictEqual(TrackAudio.needsSync(10, 10.02), false);
assert.strictEqual(TrackAudio.needsSync(10, 10.05), true);
assert.strictEqual(TrackAudio.needsSync(10, 9.9), true);
assert.strictEqual(TrackAudio.needsSync(10, 10.03, 0.03), false);
console.log("trackaudio: ok");
