"""Validate planning contracts, not application runtime behavior."""
import json
import pathlib
import re
import sqlite3
import sys

root = pathlib.Path(__file__).resolve().parents[1]
errors = []
required = ['research-decision.md', 'research.md', 'unknowns.md', 'interview.md', 'spec.md',
            'personas-and-journeys.md', 'design-system.md', 'team-review.md', 'plan.md',
            'integration-notes.md', 'db-schema.md', 'ipc-spec.md', 'mcp-spec.md',
            'operation-scenarios.md', 'qa-scenarios.md', 'checklist.md',
            'sections/index.md', 'flow-diagrams/index.md', 'domain-dictionary-delta.md']
for name in required:
    if not (root / name).is_file():
        errors.append('missing ' + name)
if errors:
    print(json.dumps({'errors': errors}, ensure_ascii=False, indent=2))
    sys.exit(1)
manifest_text = (root / 'sections/index.md').read_text(encoding='utf-8-sig')
match = re.match(r'<!-- SECTION_MANIFEST\s*\n(.*?)\nEND_MANIFEST -->', manifest_text, re.S)
manifest = match.group(1).splitlines() if match else []
if not manifest:
    errors.append('manifest absent or not first')
acceptance = {}
for index, name in enumerate(manifest, 1):
    if not re.fullmatch(r'section-' + str(index).zfill(2) + r'-[a-z-]+', name):
        errors.append('invalid/nonsequential section ' + name)
    path = root / 'sections' / (name + '.md')
    if not path.exists():
        errors.append('missing section ' + name)
        continue
    text = path.read_text(encoding='utf-8-sig')
    for heading in ['Background', 'Requirements', 'Dependencies', 'Module Contract',
                    'Reference Libraries', 'Implementation', 'Test Scenarios',
                    'Implementation Strategy', 'Quality Gate', 'Risk & Rollback',
                    'Acceptance Criteria', 'Files']:
        if '## ' + heading not in text:
            errors.append(name + ': missing ' + heading)
    for line in text.splitlines():
        if re.match(r'- \[ \] AC-', line):
            m = re.fullmatch(r'- \[ \] (AC-\d{2}-\d+) (.*?) — 근거: (.*?) — 검증: (.*)', line)
            if not m:
                errors.append('invalid AC line ' + line)
                continue
            key = m.group(1)
            if key in acceptance:
                errors.append('duplicate AC ' + key)
            if not key.startswith('AC-' + str(index).zfill(2) + '-'):
                errors.append('wrong owning section ' + key)
            acceptance[key] = (name, m.group(2), m.group(3), m.group(4))
checklist = (root / 'checklist.md').read_text(encoding='utf-8-sig')
rows = re.findall(r'^\| (AC-\d{2}-\d+) \| (.*?) \| (.*?) \| (.*?) \| (.*?) \|$', checklist, re.M)
if len(rows) != len(acceptance) or {r[0] for r in rows} != set(acceptance):
    errors.append('checklist AC union mismatch')
for key, section, statement, basis, validation in rows:
    if key in acceptance and (section, statement, basis) != acceptance[key][:3]:
        errors.append('checklist canonical wording mismatch ' + key)
qa = (root / 'qa-scenarios.md').read_text(encoding='utf-8-sig')
qa_ids = re.findall(r'^\| (QA-\d+) \|', qa, re.M)
if len(qa_ids) != len(set(qa_ids)):
    errors.append('duplicate QA IDs')
qa_refs = set(re.findall(r'AC-\d{2}-\d+', qa))
if qa_refs - set(acceptance):
    errors.append('QA unknown ACs ' + repr(sorted(qa_refs - set(acceptance))))
if set(acceptance) - qa_refs:
    errors.append('ACs missing QA mapping ' + repr(sorted(set(acceptance) - qa_refs)))
for problem in re.findall(r'^\| (P\d+) \|', (root / 'spec.md').read_text(encoding='utf-8-sig'), re.M):
    if not any(problem in row[2].split(',') for row in acceptance.values()):
        errors.append('uncovered problem ' + problem)
for expert in ['ux-analysis', 'architecture-analysis', 'redteam-analysis', 'domain-research',
               'domain-process-analysis', 'domain-technical-analysis']:
    path = root / 'team-reviews' / (expert + '.md')
    if not path.is_file() or 'Dictionary Updates' not in path.read_text(encoding='utf-8-sig'):
        errors.append('incomplete expert ' + expert)
for path in root.rglob('*.md'):
    text = path.read_text(encoding='utf-8-sig')
    for target in re.findall(r'\]\(([^)]+)\)', text):
        if re.match(r'https?://|mailto:|#', target):
            continue
        candidate = target.strip('<>').split('#')[0]
        candidate = re.sub(r':\d+$', '', candidate)
        if not candidate or not re.search(r'\.(md|mmd|sql|py|mjs)$', candidate):
            continue
        if candidate.startswith('file:'):
            errors.append('nonportable file URI in ' + str(path.relative_to(root)))
        elif not (path.parent / candidate).exists():
            errors.append('broken link ' + str(path.relative_to(root)) + ': ' + target)
db = sqlite3.connect(':memory:')
db.execute('PRAGMA foreign_keys=ON')
schema = (root / 'schema-client-v1.sql').read_text(encoding='utf-8-sig')
try:
    db.executescript(schema)
    fk = list(db.execute('PRAGMA foreign_key_check'))
    if fk:
        errors.append('DDL FK violations')
    if list(db.execute('PRAGMA integrity_check')) != [('ok',)]:
        errors.append('DDL integrity check failed')
except sqlite3.Error as exc:
    errors.append('DDL error: ' + str(exc))
result = {'scope': 'planning artifact integrity only', 'sections': len(manifest),
          'acceptance_criteria': len(acceptance), 'qa_cases': len(qa_ids),
          'diagrams': len(list((root / 'flow-diagrams').glob('*.mmd'))),
          'ddl_sqlite': sqlite3.sqlite_version, 'errors': errors,
          'application_tests': 'NOT RUN', 'performance': 'NOT RUN'}
print(json.dumps(result, ensure_ascii=False, indent=2))
sys.exit(bool(errors))
