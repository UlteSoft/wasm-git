// Dependency-free contract checks against the real installed CLI help.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {spawnSync} = require('node:child_process');
const root = path.resolve(__dirname, '..');
const context = vm.createContext({});
for (const file of ['locales.js', 'catalog.js']) vm.runInContext(fs.readFileSync(path.join(__dirname, 'static', file), 'utf8'), context);
const locales = vm.runInContext('zhCN', context);
const recipes = vm.runInContext('capabilityCatalog', context);
const placeholders = value => Array.from(value.matchAll(/\{(\w+)\}/g), m => m[1]).sort();
for (const [english, chinese] of Object.entries(locales)) {
  assert.ok(chinese.length, english);
  assert.deepEqual(placeholders(chinese), placeholders(english), `Placeholder mismatch: ${english}`);
}
for (const recipe of recipes) {
  assert.ok(Object.hasOwn(locales, recipe.group), recipe.group);
  if (recipe.description) assert.ok(Object.hasOwn(locales, recipe.description), recipe.description);
}
const client = process.env.WASM_GIT_CLIENT || path.join(root, '.tmp/target/debug/wasm-git');
const server = process.env.WASM_GIT_SERVER || path.join(root, '.tmp/target/debug/wasm-git-server');
function nativeHelp(binary) {
  const result = spawnSync(binary, ['--help'], {encoding: 'utf8', timeout: 10000});
  assert.equal(result.status, 0, result.stderr);
  return result.stdout + result.stderr;
}
const help = nativeHelp(client);
// bisect's alternatives describe nested actions, not additional root commands.
const rootUsage = help.replace(/^  bisect .*$/gm, '  bisect <DIR>');
const usage = [...rootUsage.matchAll(/(?:^  |\| )([a-z][a-z-]*(?:\|[a-z-]+)*) (?=[<[\-])/gm)].flatMap(m => m[1].split('|'));
const storage = help.split('\n').find(line => line.startsWith('Additional storage commands:'));
assert.ok(storage, 'Storage command discovery failed');
const commands = [...new Set([...usage, ...storage.split('. Signing')[0].replace('Additional storage commands: ', '').split('; ').map(s => s.split(' ')[0])])].filter(c => !['help', 'server'].includes(c));
assert.ok(commands.length > 60, 'CLI help command discovery failed');
const covered = new Set(recipes.filter(r => r.target === 'client').map(r => r.command.split(' ')[0]));
assert.deepEqual(commands.filter(c => !covered.has(c)), [], 'Native commands missing from the capability catalog');
const serverHelp = nativeHelp(server);
for (const recipe of recipes.filter(r => r.target === 'server')) assert.ok(serverHelp.includes(recipe.command));
console.log(`${Object.keys(locales).length} bilingual messages; ${recipes.length} forms cover all ${commands.length} client root commands and server administration.`);
