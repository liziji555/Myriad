import { createContext, useContext } from "react";
import type { Bot, BotId } from "../api/types";

const BotDirectoryContext = createContext<readonly Bot[]>([]);

export const BotDirectoryProvider = BotDirectoryContext.Provider;

export function useBotById(id: BotId | string): Bot | undefined {
  const bots = useContext(BotDirectoryContext);
  return bots.find((bot) => bot.id === id);
}
