"""C1/C2：配方下发与不可变版本策略。"""

from .registry import REGISTRY, ModuleSpec, get_module, register, registered_modules
from .schema import Recipe, validate_recipe

__all__ = [
    "REGISTRY",
    "ModuleSpec",
    "Recipe",
    "get_module",
    "register",
    "registered_modules",
    "validate_recipe",
]
