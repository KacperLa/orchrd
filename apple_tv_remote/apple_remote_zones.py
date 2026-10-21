"""Edge scroll strips and the corner right-click for the Siri Remote touch pad.

The outer strip on the right scrolls vertically; the outer strip at the top
scrolls horizontally. Their intersection is the corner that turns a mechanical
click into a right click.

The pad does NOT report coordinates centered on zero, so the strips are defined
against the measured extent of reachable finger positions (see ZoneConfig).
"""

from dataclasses import dataclass
from enum import Enum


class Region(Enum):
    CENTER = "center"
    RIGHT = "right"
    TOP = "top"
    CORNER = "corner"


@dataclass(frozen=True)
class ZoneConfig:
    edge_mm: float = 4.0
    scroll_step_mm: float = 3.0
    # Measured by sweeping an A1513 pad to all four extremes: x and y grow
    # toward the right and top, and the pad's box is far from the origin.
    x_min: int = -1818
    x_max: int = -182
    y_min: int = -1074
    y_max: int = 614


class TouchZones:
    def __init__(self, pad_size_mm: float, config: ZoneConfig = ZoneConfig()) -> None:
        if config.x_max <= config.x_min or config.y_max <= config.y_min:
            raise ValueError("pad bounds must have max greater than min")
        self.config = config
        self.mm_per_x = pad_size_mm / (config.x_max - config.x_min)
        self.mm_per_y = pad_size_mm / (config.y_max - config.y_min)
        self.horizontal_mm = 0.0
        self.vertical_mm = 0.0

    def region(self, x: int, y: int) -> Region:
        """Classify a raw pad coordinate; the remote's Y grows toward the top."""
        right = self.right_edge_mm(x) <= self.config.edge_mm
        top = self.top_edge_mm(y) <= self.config.edge_mm
        if right and top:
            return Region.CORNER
        if right:
            return Region.RIGHT
        if top:
            return Region.TOP
        return Region.CENTER

    def right_edge_mm(self, x: int) -> float:
        """Distance inside the pad from the right edge; clamps outside the pad."""
        return max(0.0, (self.config.x_max - x) * self.mm_per_x)

    def top_edge_mm(self, y: int) -> float:
        return max(0.0, (self.config.y_max - y) * self.mm_per_y)

    def scroll(self, region: Region, dx_mm: float, dy_mm: float) -> tuple[int, int]:
        """Consume screen-space pad motion and return whole (horizontal, vertical) notches."""
        # Natural scrolling: the content follows the finger.
        if region in (Region.RIGHT, Region.CORNER):
            self.vertical_mm += dy_mm
        if region in (Region.TOP, Region.CORNER):
            self.horizontal_mm -= dx_mm
        step = self.config.scroll_step_mm
        horizontal = int(self.horizontal_mm / step)
        vertical = int(self.vertical_mm / step)
        self.horizontal_mm -= horizontal * step
        self.vertical_mm -= vertical * step
        return horizontal, vertical

    def reset(self) -> None:
        self.horizontal_mm = self.vertical_mm = 0.0
