#!/usr/bin/env python3
"""Update the three existing EC2 checkouts and figure-tools after the old run exits.

All local file merges are checked in temporary files before changing any checkout.
No stash, reset --hard, force push, AWS calls or experiment execution.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tempfile
import zipfile


def git(repo, *args, check=True):
    result = subprocess.run(['git', '-C', str(repo), *args], capture_output=True)
    if check and result.returncode:
        raise RuntimeError(result.stderr.decode('utf-8', 'replace').strip())
    return result


def names(data):
    return {name.decode('utf-8') for name in data.split(b'\0') if name}


def prepare_repository(repo, entry, stage):
    if not (repo / '.git').exists():
        raise RuntimeError(f'仓库不存在：{repo}')
    if git(repo, 'ls-files', '-u').stdout:
        raise RuntimeError(f'{repo}: 已存在未解决的 Git 冲突；本脚本没有修改文件。')
    for state in ('MERGE_HEAD', 'CHERRY_PICK_HEAD', 'rebase-merge', 'rebase-apply'):
        location = git(repo, 'rev-parse', '--git-path', state).stdout.decode().strip()
        path = Path(location)
        if (path if path.is_absolute() else repo / path).exists():
            raise RuntimeError(f'{repo}: 尚有未结束的 Git 操作；本脚本没有修改文件。')
    if git(repo, 'diff', '--cached', '--name-only').stdout:
        raise RuntimeError(f'{repo}: 存在已暂存的修改，请先完成该提交；本脚本没有修改文件。')
    before = git(repo, 'rev-parse', 'HEAD').stdout.decode().strip()
    print(f'Fetch {entry["name"]}: {entry["branch"]}', flush=True)
    git(repo, 'fetch', '--no-tags', entry['url'], entry['branch'])
    revision = entry['revision']
    if git(repo, 'merge-base', '--is-ancestor', revision, 'FETCH_HEAD', check=False).returncode:
        raise RuntimeError(f'{entry["name"]}: 发行提交尚未发布到远端分支，请先完成发布；本地文件未修改。')
    if git(repo, 'merge-base', '--is-ancestor', before, revision, check=False).returncode:
        raise RuntimeError(f'{entry["name"]}: 当前 HEAD 不是本次发行提交的祖先，不自动切换或丢弃本地提交。')
    local = names(git(repo, 'diff', 'HEAD', '--name-only', '-z').stdout)
    upstream = names(git(repo, 'diff', '--name-only', '-z', before, revision).stdout)
    tracked_next = names(git(repo, 'ls-tree', '-r', '--name-only', '-z', revision).stdout)
    untracked = names(git(repo, 'ls-files', '--others', '--exclude-standard', '-z').stdout)
    if untracked & tracked_next:
        raise RuntimeError(f'{repo}: 新版受控文件与现有未跟踪文件重名，未修改仓库：{sorted(untracked & tracked_next)}')
    merged, originals = {}, {}
    for name in sorted(local & upstream):
        path = repo / name
        if path.is_symlink() or not path.is_file() or name not in tracked_next:
            raise RuntimeError(f'{repo}: 无法自动保留本地新增/删除/链接文件 {name}；仓库未修改。')
        original = path.read_bytes()
        ancestor = git(repo, 'show', f'{before}:{name}').stdout
        incoming = git(repo, 'show', f'{revision}:{name}').stdout
        if b'\0' in original + ancestor + incoming:
            raise RuntimeError(f'{repo}: {name} 是二进制修改，仓库未修改。')
        paths = [stage / part for part in ('local', 'base', 'incoming')]
        for temp, value in zip(paths, (original, ancestor, incoming)):
            temp.write_bytes(value)
        result = subprocess.run(['git', 'merge-file', '-p', *map(str, paths)], capture_output=True)
        if result.returncode:
            raise RuntimeError(f'{repo}: {name} 的本地修改与新版同一位置不一致。预检查已停止；'
                               '原文件保持原样，未写入冲突标记，也未创建 Git 合并状态。')
        merged[name], originals[name] = result.stdout, original
    return dict(repo=repo, entry=entry, before=before, local=local,
                merged=merged, originals=originals,
                patch=git(repo, 'diff', '--binary', 'HEAD').stdout)


def apply_repository(plan, backups):
    repo, entry = plan['repo'], plan['entry']
    backup = backups / entry['name']
    backup.mkdir(parents=True)
    (backup / 'HEAD.txt').write_text(plan['before'] + '\n', encoding='utf-8')
    (backup / 'local-changes.patch').write_bytes(plan['patch'])
    for name in plan['local']:
        path = repo / name
        if path.is_file() and not path.is_symlink():
            target = backup / 'files' / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
    settings = repo / 'benchmark/settings.json'
    if settings.is_file():
        shutil.copy2(settings, backup / 'settings.json')
    overlaps = list(plan['merged'])
    try:
        if overlaps:
            git(repo, 'restore', '--source=HEAD', '--worktree', '--', *overlaps)
        git(repo, 'checkout', '--detach', entry['revision'])
    except Exception:
        for name, content in plan['originals'].items():
            (repo / name).write_bytes(content)
        raise
    for name, content in plan['merged'].items():
        (repo / name).write_bytes(content)
    if settings.is_file():
        config = json.loads(settings.read_text(encoding='utf-8-sig'))
        if isinstance(config.get('repo'), dict) and config['repo'].get('branch') != entry['branch']:
            config['repo']['branch'] = entry['branch']
            settings.write_text(json.dumps(config, indent=2) + '\n', encoding='utf-8')
    assert not git(repo, 'ls-files', '-u').stdout
    print(f'{entry["name"]}: {entry["revision"]}; local parameter changes retained', flush=True)


def prepare_tools(distribution, manifest, stage):
    path = distribution / manifest['toolkit']['file']
    if hashlib.sha256(path.read_bytes()).hexdigest() != manifest['toolkit']['sha256']:
        raise RuntimeError('工具包 ZIP 的 SHA256 不匹配。')
    with zipfile.ZipFile(path) as archive:
        if archive.testzip():
            raise RuntimeError('工具包 ZIP 损坏。')
        for name in archive.namelist():
            item = PurePosixPath(name)
            if item.is_absolute() or '..' in item.parts or not item.parts or item.parts[0] != 'five-protocol-tools':
                raise RuntimeError(f'ZIP 路径不合法：{name}')
        verification = json.loads(archive.read('five-protocol-tools/verification.json'))
        if verification['release'] != 'v3.5-a1-crash-aba-a5-only':
            raise RuntimeError('工具包版本不符。')
        for name, expected in verification['sha256'].items():
            if hashlib.sha256(archive.read('five-protocol-tools/' + name)).hexdigest() != expected:
                raise RuntimeError(f'工具包内容校验失败：{name}')
        archive.extractall(stage)
    return stage / 'five-protocol-tools'


def update(root, distribution, check_only=False):
    manifest = json.loads((distribution / 'release-v3.5.json').read_text(encoding='utf-8'))
    with tempfile.TemporaryDirectory(prefix='figure-v35-stage-', dir=root) as temporary:
        stage = Path(temporary)
        fresh = prepare_tools(distribution, manifest, stage)
        # Fetching is read-only for checkout contents. Finish all three merge
        # checks and ZIP validation before the first existing file is replaced.
        plans = [prepare_repository(root / entry['directory'], entry, stage)
                 for entry in manifest['repositories']]
        if check_only:
            print('预检查通过；未切换仓库或替换工具。')
            return
        stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        backups = root / 'update-backups' / ('v3.5-' + stamp)
        backups.mkdir(parents=True)
        for plan in plans:
            apply_repository(plan, backups)
        target = root / 'figure-tools'
        if target.exists():
            target.rename(backups / 'figure-tools')
        fresh.rename(target)
        print(f'v3.5 更新完成。备份：{backups}')
        print('下一步：fab -r /home/ubuntu/experiments/figure-tools matrix --dry-run')


def main():
    import fcntl
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path('/home/ubuntu/experiments'))
    parser.add_argument('--distribution', type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument('--check-only', action='store_true')
    args = parser.parse_args()
    with open('/tmp/five-protocol-figure-matrix.lock', 'a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit('实验仍在运行；请等当前批处理退出并清理实例后更新。')
        try:
            update(args.root.resolve(), args.distribution.resolve(), args.check_only)
        except (RuntimeError, OSError, ValueError, KeyError) as error:
            raise SystemExit(str(error))


if __name__ == '__main__':
    main()
