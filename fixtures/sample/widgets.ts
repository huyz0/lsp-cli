export class WidgetRenderer {
  renderWidget(widget: Widget): string {
    return `<div>${widget.label}</div>`;
  }

  computeTotalArea(widgets: Widget[]): number {
    return widgets.reduce((sum, w) => sum + w.area(), 0);
  }
}

export interface Widget {
  label: string;
  area(): number;
}
