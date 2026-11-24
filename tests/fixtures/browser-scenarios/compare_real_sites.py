"""Real-site tasks done by both tools.
usage: real_tasks.py ab OUT.json            run agent-browser on every task
       real_tasks.py gen OUT.json [retry]   write the front_explore script (retry: names of steps to give nth 1)
       real_tasks.py sum EXPLORE.log        read the explore log into per-task figures
"""
import json, os, re, subprocess, sys, time
B = 'https://getbootstrap.com/docs/5.3/components/'
M = 'https://mui.com/material-ui/'
T = 'https://the-internet.herokuapp.com/'
# (name, url, steps, text the page must show at the end). A step: (verb, name, value)
TASKS = [
 ('bs-modal', B + 'modal/', [('click', 'Launch static backdrop modal', None)], 'Understood'),
 ('bs-accordion', B + 'accordion/', [('click', 'Accordion Item #2', None)], "second item"),
 ('mui-select', M + 'react-select/', [('click', 'Age', None), ('click', 'Twenty', None)], 'Twenty'),
 ('mui-dialog', M + 'react-dialog/', [('click', 'Open simple dialog', None), ("click", "username@gmail.com", None)], "Selected: username@gmail.com"),
 ('mui-autocomplete', M + 'react-autocomplete/', [('fill', 'Movie', 'god'), ('click', 'The Godfather', None)], 'The Godfather'),
 ('todomvc', 'https://todomvc.com/examples/react/dist/', [('fill', 'New Todo Input', 'walk dog'), ('press', 'New Todo Input', 'Enter')], '1 item left'),
 ('dynamic', T + 'dynamic_loading/2', [('click', 'Start', None), ('wait', 'Hello World!', None)], 'Hello World!'),
 ('add-remove', T + 'add_remove_elements/', [('click', 'Add Element', None), ('click', 'Add Element', None), ('click', 'Delete', None)], 'Delete'),
 ('wikipedia', 'https://en.wikipedia.org/wiki/Main_Page', [('fill', 'Search Wikipedia', 'Ada Lovelace'), ('press', 'Search Wikipedia', 'Enter')], 'Analytical Engine'),
 ('datatables', 'https://datatables.net/examples/basic_init/zero_configuration.html', [('fill', 'Search:', 'London')], 'filtered from 57'),
 ('shop', 'https://demowebshop.tricentis.com/books', [('click', 'Add to cart', None), ('click', 'Shopping cart', None), ('click', 'I agree with the terms of service and I adhere to them unconditionally', None), ('click', 'Checkout', None), ('click', 'Checkout as Guest', None)], 'Billing address'),
 ('books', 'https://books.toscrape.com/', [('click', 'Mystery', None), ('click', 'next', None)], 'Page 2 of 2'),
]
mode = sys.argv[1]
if mode == 'ab':
    BIN = os.path.expanduser('~/.local/bin/agent-browser')
    ENV = dict(AGENT_BROWSER_PROXY='http://192.168.10.100:36699', HOME=os.environ['HOME'], PATH='/usr/bin:/bin',
               LANG='C.UTF-8', AGENT_BROWSER_NO_VERSION_CHECK='1')
    LINE = re.compile(r'^\s*- (\w+) "([^"]*)"(.*)$')
    # Where the snapshot gives a control no name, an agent goes by the hint beside it.
    HINTS = {'I agree with the terms of service and I adhere to them unconditionally': 'name:termsofservice'}
    def find(snapshot, name):
        hint = HINTS.get(name)
        if hint:
            for line in snapshot.split('\n'):
                ref = re.search(r'ref=(e\d+)', line)
                if ref and hint in line: return ref.group(1)
        for line in snapshot.split('\n'):
            m = LINE.match(line)
            if not m: continue
            ref = re.search(r'ref=(e\d+)', m.group(3))
            # An agent reading the snapshot takes a name that differs only in case: a style may show it in capitals.
            if ref and m.group(2).lower() == name.lower() and m.group(1) not in ('generic', 'heading'): return ref.group(1)
    report = {}
    for name, url, steps, expect in TASKS:
        calls = size = 0; log = []
        def ab(*args, count=True):
            global calls, size
            try:
                done = subprocess.run([BIN, '--session', 'r' + name.replace('-', ''), '--executable-path', '/opt/google/chrome/chrome', *args],
                                      env=ENV, capture_output=True, text=True, timeout=60)
                code, out = done.returncode, done.stdout + done.stderr
            except subprocess.TimeoutExpired:
                code, out = 1, 'timeout'
            if count:
                calls += 1; size += len(out.encode()); log.append((' '.join(args)[:80], code, len(out.encode())))
            return code, out
        began = time.time()
        ab('open', url)
        left, stuck, note = list(steps), 0, ''
        while left and stuck < 4:
            verb, target, value = left[0]
            if verb == 'wait':
                ab('wait', '--text', target, '--timeout', '8000'); left.pop(0); continue
            _, snapshot = ab('snapshot', '-i')
            ref = find(snapshot, target)
            if not ref:
                stuck += 1; note = f'{target!r} is not in the snapshot'; time.sleep(0.5); continue
            cmd = {'click': ['click', '@' + ref], 'fill': ['fill', '@' + ref, value or ''], 'press': ['press', value or '']}[verb]
            if verb == 'press': ab('focus', '@' + ref)
            code, out = ab(*cmd)
            if code: stuck += 1; note = out.strip()[:80]
            else: stuck = 0; left.pop(0)
        _, last = ab('snapshot', '-i')
        elapsed = int((time.time() - began) * 1000)
        seen = expect in last
        if not seen:
            _, whole = ab('snapshot', count=False); shown = expect in whole
        else: shown = True
        ab('close', count=False)
        report[name] = dict(calls=calls, bytes=size, ms=elapsed, done=not left, seen=seen, shown=shown, note=note, log=log)
        print(name, json.dumps({k: v for k, v in report[name].items() if k != 'log'}), flush=True)
    json.dump(report, open(sys.argv[2], 'w'), indent=1)
elif mode == 'gen':
    retry = set(sys.argv[3].split(',')) if len(sys.argv) > 3 and sys.argv[3] else set()
    calls = []
    for name, url, steps, expect in TASKS:
        def step(verb, target, value, nth=False):
            t = {'name': target}
            if nth: t['nth'] = 1
            if verb == 'wait': return {'wait': {'text': target}}
            if verb == 'fill': return {'fill': t, 'text': value}
            if verb == 'press': return {'press': value, 'on': t}
            return {verb: t}
        todo = [{'open': url}]
        for i, (verb, target, value) in enumerate(steps):
            key = f'{name}:{i}'
            if key in retry:
                # The call made before the answer said which matches there are, then the rest again.
                calls.append(['browser_do', {'steps': todo + [step(verb, target, value)]}])
                todo = [step(verb, target, value, True)]
            else:
                todo.append(step(verb, target, value))
        calls.append(['browser_do', {'steps': todo}])
        calls.append(['browser_read', {'what': 'page', 'find': expect, 'max_chars': 400}])
        calls.append(['browser_do', {'steps': [{'close': True}]}])
    json.dump(calls, open(sys.argv[2], 'w'))
else:
    text = open(sys.argv[2], encoding='utf-8', errors='replace').read()
    parts = re.split(r'^=== ', text, flags=re.M)[1:]
    it = iter(parts); report = {}; retry = []
    for name, url, steps, expect in TASKS:
        calls = size = ms = 0; seen = shown = False; fails = []
        for part in it:
            head, _, rest = part.partition('\n')
            m = re.match(r'--- (\d+) bytes, (\d+) ms\n', rest)
            body = rest[m.end():] if m else rest
            if head.startswith('browser_do') and '"close"' in head: break
            if head.startswith('browser_read'):
                shown = expect in body; continue
            calls += 1; size += int(m.group(1)); ms += int(m.group(2))
            seen = seen or expect in body
            for f in re.findall(r'^✗ (\d+) (\w+): (\w+)', body, flags=re.M): fails.append(f)
        report[name] = dict(calls=calls, bytes=size, ms=ms, seen=seen, shown=shown or seen, fails=fails)
        print(name, json.dumps(report[name]))
    json.dump(report, open(sys.argv[2] + '.json', 'w'), indent=1)
