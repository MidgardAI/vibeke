import { describe, expect, test } from 'bun:test';
import type { BrowseEntry } from '@vibeke/core';
import { browseArgs, commonPrefix, complete, descend, filterEntries, parentOf, splitPath } from '../src/lib/path-picker';

const dirs = (...names: string[]): BrowseEntry[] => names.map((name) => ({ name, git_repo: name.startsWith('repo') }));

describe('path picker', () => {
  test('splits the typed path into the folder to list and the name being typed', () => {
    expect(splitPath('')).toEqual({ dir: '~/', prefix: '' });
    expect(splitPath('~')).toEqual({ dir: '~/', prefix: '' });
    expect(splitPath('~/code/vi')).toEqual({ dir: '~/code/', prefix: 'vi' });
    expect(splitPath('~/code/')).toEqual({ dir: '~/code/', prefix: '' });
    expect(splitPath('/srv')).toEqual({ dir: '/', prefix: 'srv' });
    expect(splitPath('code')).toEqual({ dir: '~/', prefix: 'code' });
  });

  test('dot-folders are requested only for a name starting with a dot', () => {
    expect(browseArgs('~/code/vi')).toEqual({ path: '~/code/' });
    expect(browseArgs('~/.co')).toEqual({ path: '~/', prefix: '.' });
  });

  test('filters with the fuzzy matcher, best match first', () => {
    const e = dirs('alpha', 'vibeke', 'vim', 'nvim-config', '.vscode');
    expect(filterEntries(e, '').map((x) => x.name)).toEqual(['alpha', 'vibeke', 'vim', 'nvim-config']);
    expect(filterEntries(e, 'vim').map((x) => x.name)).toEqual(['vim', 'nvim-config']);
    expect(filterEntries(e, 'vbk').map((x) => x.name)).toEqual(['vibeke']);
    expect(filterEntries(e, '.vs').map((x) => x.name)).toEqual(['.vscode']);
    expect(filterEntries(e, 'zzz')).toEqual([]);
  });

  test('tab completes one match to the folder, several to their common prefix', () => {
    const e = dirs('vibeke', 'vibeke-site', 'other');
    expect(complete('~/code/o', e)).toBe('~/code/other/');
    expect(complete('~/code/v', e)).toBe('~/code/vibeke');
    // No progress (already at the common prefix) and no match: nothing to do.
    expect(complete('~/code/vibeke', e)).toBeNull();
    expect(complete('~/code/x', e)).toBeNull();
    expect(complete('~/code/V', dirs('Vibeke', 'vim'))).toBe('~/code/Vi');
    expect(commonPrefix(['Vibeke', 'vim'])).toBe('Vi');
    expect(commonPrefix([])).toBe('');
  });

  test('descends into a folder and goes back up', () => {
    expect(descend('~/code/vi', 'vibeke')).toBe('~/code/vibeke/');
    expect(descend('~/', 'code')).toBe('~/code/');
    expect(parentOf('~/code/vibeke/')).toBe('~/code/');
    expect(parentOf('~/code/vibeke/sr')).toBe('~/code/');
    expect(parentOf('~/')).toBe('~/');
    expect(parentOf('/srv/')).toBe('/');
    expect(parentOf('/')).toBe('/');
  });
});
