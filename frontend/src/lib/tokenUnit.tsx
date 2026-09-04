// Token 展示单位全局状态：默认“万”，localStorage 持久化，Dashboard / Analytics 共享。

import { createContext, useContext, useMemo, useState, type ReactNode } from 'react';
import type { TokenUnit } from './format';

const STORAGE_KEY = 'llm-router.token-unit';

const UNITS: { value: TokenUnit; label: string }[] = [
  { value: 'wan', label: '万' },
  { value: 'yi', label: '亿' },
  { value: 'm', label: 'M' },
  { value: 'raw', label: '原始' },
];

function loadInitialUnit(): TokenUnit {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY);
    if (saved && UNITS.some((u) => u.value === saved)) return saved as TokenUnit;
  } catch {
    // localStorage 不可用（隐私模式等）时用默认单位
  }
  return 'wan';
}

type TokenUnitContextValue = { unit: TokenUnit; setUnit: (unit: TokenUnit) => void };

const TokenUnitContext = createContext<TokenUnitContextValue>({ unit: 'wan', setUnit: () => {} });

export function TokenUnitProvider({ children }: { children: ReactNode }) {
  const [unit, setUnitState] = useState<TokenUnit>(loadInitialUnit);
  const value = useMemo<TokenUnitContextValue>(() => ({
    unit,
    setUnit: (next) => {
      setUnitState(next);
      try {
        window.localStorage.setItem(STORAGE_KEY, next);
      } catch {
        // ignore: 持久化失败不影响本次会话内的切换
      }
    },
  }), [unit]);
  return <TokenUnitContext.Provider value={value}>{children}</TokenUnitContext.Provider>;
}

export function useTokenUnit(): TokenUnitContextValue {
  return useContext(TokenUnitContext);
}

/// 工具栏里的单位切换下拉框；配合 <div className="field"><label>…</label> 使用。
export function TokenUnitSelect() {
  const { unit, setUnit } = useTokenUnit();
  return (
    <select value={unit} onChange={(e) => setUnit(e.target.value as TokenUnit)} aria-label="Token 显示单位">
      {UNITS.map((u) => <option key={u.value} value={u.value}>{u.label}</option>)}
    </select>
  );
}
