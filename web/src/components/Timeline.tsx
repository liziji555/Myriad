interface Props {
  onLightDemo(source: HTMLElement): void;
}

export function Timeline({ onLightDemo }: Props) {
  return (
    <section className="sa-timeline" aria-label="时间线">
      <div className="sa-empty">
        <h2>这个房间还没有消息</h2>
        <p>对话功能还没有接入。现在可以先看看「过程连到结果」的光线效果：</p>
        <button type="button" className="sa-button" onClick={(event) => onLightDemo(event.currentTarget)}>
          演示光线
        </button>
      </div>
    </section>
  );
}
