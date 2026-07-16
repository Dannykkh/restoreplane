import React from "react";
import ReactDOM from "react-dom/client";

// 폰트: npm self-host(오프라인 데스크톱). 쓰는 weight만 로드(DESIGN.md).
// Pretendard는 단일 가변폰트(dynamic subset) — weight 축 하나라 미사용 weight 파일이 없다 + 한글 유니코드 서브셋 lazy 로드.
import "pretendard/dist/web/variable/pretendardvariable-dynamic-subset.css";
import "@fontsource/space-grotesk/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";

import "./styles/tokens.css";
import "./styles/global.css";

import { App } from "./App";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
