import { createContext, useContext, useEffect, useRef, type ReactNode } from "react";
import { createLightLayer, type Flight, type FlightResult, type LightLayer } from "../vendor/light-ui";

type Emit = (flight: Flight) => Promise<FlightResult>;

const LightContext = createContext<Emit>(() => Promise.resolve("cancelled"));

/** 全页面共用一个光线图层。只在真实事件发生时调用 emit，不把每帧进度放进 React 状态。 */
export function LightProvider({ children }: { children: ReactNode }) {
  const layer = useRef<LightLayer | null>(null);

  useEffect(() => {
    const created = createLightLayer();
    layer.current = created;
    return () => {
      created.destroy();
      layer.current = null;
    };
  }, []);

  const emit = useRef<Emit>((flight) => layer.current?.emit(flight).finished ?? Promise.resolve("cancelled"));

  return <LightContext.Provider value={emit.current}>{children}</LightContext.Provider>;
}

export function useLight(): Emit {
  return useContext(LightContext);
}
