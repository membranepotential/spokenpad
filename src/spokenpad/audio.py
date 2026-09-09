"""Audio array types shared by the offline Python reference tools.

The production capture implementation is Rust. Importing this module must not
open or initialize PortAudio; it is deliberately only a precise NumPy type.
"""

from __future__ import annotations

import numpy as np
import numpy.typing as npt

type MonoAudio = npt.NDArray[np.float32]
