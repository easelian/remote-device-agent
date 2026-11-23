#!/usr/bin/env python3
"""The scenario tasks of front_pool, done with agent-browser the way an agent
that knows it well would: one snapshot, then every step that snapshot can
name in one batch, then a snapshot to see the outcome.

Usage: compare_agent_browser.py PORT [scenario,...]

The scenario site must be running on PORT (server.py PORT). AGENT_BROWSER names
the agent-browser binary, RDH_BROWSER_EXECUTABLE the browser it drives, and
COMPARE_REPORT a file to write the result of every task to. The numbers to set
beside these are those of front_pool in tests/browser_scenarios.rs."""
import json, os, re, subprocess, sys, time, urllib.request

PORT = sys.argv[1]
ONLY = sys.argv[2].split(',') if len(sys.argv) > 2 else None
BIN = os.environ.get('AGENT_BROWSER') or os.path.expanduser('~/.local/bin/agent-browser')
BROWSER = os.environ.get('RDH_BROWSER_EXECUTABLE', '/opt/google/chrome/chrome')
ENV = dict(HOME=os.environ['HOME'], PATH='/usr/bin:/bin', LANG='C.UTF-8', AGENT_BROWSER_NO_VERSION_CHECK='1')

def click(name, within=None): return ('click', name, within, None)
def approve(order): return click('Approve', order)
def fill(name, text): return ('fill', name, None, text)
def select(name, value): return ('select', name, None, value)
def check(name): return ('check', name, None, None)
def wait(text): return ('wait', text, None, None)
def scroll(by, within=None): return ('scroll', within, None, by)
READY = wait('Orders are ready')
TASKS = [
    ('recycle', [scroll(400, 'Order list'), approve('Order 0012')], ['12']),
    ('rerender', [click('Remove', 'Item 2'), click('Remove', 'Item 4')], ['2', '4']),
    ('css-state', [check('Show advanced'), select('Plan', 'pro')], []),
    ('modal', [click('Delete', 'Order B42'), click('Confirm')], ['B42']),
    ('optimistic', [approve('Order A17'), wait('Rejected')], ['A17']),
    ('spa', [click('Orders'), approve('Order S03')], ['S03']),
    ('ticker', [approve('Order T02')], ['Order T02']),
    ('dependent', [select('Country', 'jp'), select('City', 'Osaka'), click('Save address')], ['jp/Osaka']),
    ('sort', [click('Sort by amount'), approve('Invoice Q3')], ['Q3']),
    ('pager', [click('Next page'), approve('Order P07')], ['P07']),
    ('feed', [approve('Order F1')], ['F1']),
    ('worker', [approve('Order W1')], ['W1']),
    ('reload', [READY, approve('Order L1')], ['L1']),
    ('churn', [READY, approve('Order C1')], ['C1']),
    ('shadow', [approve('Order K2'), approve('Order K3')], ['K2', 'K3']),
    ('xframe', [wait('Order X2'), approve('Order X2')], ['X2']),
    ('toplayer', [click('Open settings'), click('Save settings')], ['save']),
    ('inline', [click('Edit', 'R2'), fill('Name for R2', 'Zed'), click('Save')], ['R2=Zed']),
    ('wizard', [fill('Email', 'a@b.test'), click('Next'), select('Plan', 'pro'), click('Next'), click('Create account')], ['a@b.test/pro']),
    ('autocomplete', [fill('City', 'Pa'), wait('Paris'), click('Paris')], ['Paris']),
    ('kanban', [click('Move to Done', 'Card K2'), click('Move to Done', 'Card K3')], ['K2', 'K3']),
    ('sticky', [approve('Order S45'), approve('Order S44'), approve('Order S02')], ['S45', 'S44', 'S02']),
    ('lazy', [scroll(1500), wait('Order L35'), approve('Order L35')], ['L35']),
    ('big', [approve('Order G1500')], ['G1500']),
]

def writes(name):
    return json.load(urllib.request.urlopen(f'http://127.0.0.1:{PORT}/audit')).get(name, [])

class Run:
    def __init__(self, session):
        self.session, self.calls, self.bytes, self.log = session, 0, 0, []
    def ab(self, *args, count=True):
        done = subprocess.run([BIN, '--session', self.session, '--executable-path', BROWSER, *args],
                              env=ENV, capture_output=True, text=True, timeout=60)
        out = done.stdout + done.stderr
        if count:
            self.calls += 1; self.bytes += len(out.encode())
            self.log.append((' '.join(args)[:120], done.returncode, len(out.encode())))
        return done.returncode, out

LINE = re.compile(r'^\s*- (\w+) "([^"]*)"(.*)$')
def find(snapshot, name, within):
    """The ref of the element a step names, as an agent reading the snapshot would pick it."""
    found = []
    for line in snapshot.split('\n'):
        m = LINE.match(line)
        if not m: continue
        ref = re.search(r'ref=(e\d+)', m.group(3))
        if not ref or m.group(2) != name or m.group(1) in ('generic', 'heading'): continue
        hint = re.search(r'hint="(?:[^"]*; )?in:([^";]*)', m.group(3))
        found.append((ref.group(1), hint.group(1) if hint else ''))
    if within and len(found) > 1:
        # A lone candidate carries no hint: there is nothing to tell it from.
        found = [f for f in found if within in f[1]]
    return found[0][0] if found else None

def command(step, snapshot):
    verb, name, within, value = step
    if verb == 'wait': return ['wait', '--text', name, '--timeout', '5000']
    if verb == 'scroll' and name is None: return ['scroll', 'down', str(value)]
    ref = find(snapshot, name, within)
    if verb == 'scroll':
        return ['scroll', 'down', str(value), '--selector', '@' + ref] if ref else None
    if not ref: return None
    return {'click': ['click', '@' + ref], 'check': ['check', '@' + ref],
            'fill': ['fill', '@' + ref, value], 'select': ['select', '@' + ref, value]}[verb]

def quoted(args): return ' '.join(a if re.fullmatch(r'[\w@.+-]+', a) else json.dumps(a) for a in args)

report = {}
for name, steps, expected in TASKS:
    if ONLY and name not in ONLY: continue
    run = Run('task' + name.replace('-', ''))
    began = time.time()
    run.ab('open', f'http://127.0.0.1:{PORT}/{name}.html')
    left, stuck, note = list(steps), 0, ''
    while left and stuck < 4:
        # Steps that need no snapshot go first, alone: a wait or a page scroll.
        if left[0][0] == 'wait' or (left[0][0] == 'scroll' and left[0][1] is None):
            code, out = run.ab(*command(left[0], ''))
            # An agent whose wait runs out looks at the page and goes on.
            if code != 0: note = out.strip()[:80]
            left.pop(0)
            continue
        _, snapshot = run.ab('snapshot', '-i')
        batch = []
        for step in left:
            if step[0] == 'wait' or (step[0] == 'scroll' and step[1] is None): break
            cmd = command(step, snapshot)
            if not cmd: break
            batch.append(cmd)
        if not batch:
            stuck += 1; note = f'{left[0][1]!r} is not in the snapshot'; time.sleep(0.5); continue
        if len(batch) == 1:
            code, out = run.ab(*batch[0]); ok = 1 if code == 0 else 0
        else:
            code, out = run.ab('batch', '--bail', *[quoted(c) for c in batch])
            ok = out.count('✓') if code else len(batch)
        if ok == 0: stuck += 1; note = out.strip()[:80]
        else: stuck = 0
        del left[:ok]
    run.ab('snapshot', '-i')  # what the agent reads to learn the outcome
    elapsed = int((time.time() - began) * 1000)
    time.sleep(0.4)
    got = writes(name)
    run.ab('close', count=False)
    report[name] = dict(calls=run.calls, bytes=run.bytes, ms=elapsed, ok=not left and got == expected,
                        writes=got, note=note, log=run.log)
    print(name, json.dumps({k: v for k, v in report[name].items() if k != 'log'}), flush=True)
if os.environ.get('COMPARE_REPORT'):
    json.dump(report, open(os.environ['COMPARE_REPORT'], 'w'), indent=1)
done = [r for r in report.values()]
print('TOTAL', len(done), 'ok', sum(r['ok'] for r in done), 'calls', sum(r['calls'] for r in done),
      'bytes', sum(r['bytes'] for r in done), 'ms', sum(r['ms'] for r in done))
